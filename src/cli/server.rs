//! `roster server start` — the composition root of the one daemon: bring up
//! the governed-egress gateway, the channel listeners, and the task-dispatch
//! loop as supervised siblings in one process (one thing to start, one thing
//! to restart after a rebuild). The machinery lives in its blocks; this file
//! only wires it. And `roster server status` — health, computed, never
//! model-written.

use crate::action::gate;
use crate::util::BErr;
use crate::work::tms;
use std::collections::BTreeMap;
use std::time::Duration;

const BUILD: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("ROSTER_BUILD"), ")");

pub async fn run(cap: usize, once: bool, no_listen: bool, addr: Option<&str>) -> Result<(), BErr> {
    // Refuse to boot on broken config — better loud at start than silently
    // denying everything. Mid-flight breakage fails closed instead (the
    // gateway denies, dispatch pauses) so a bad edit never kills the daemon.
    if let Err(errors) = crate::config::load() {
        for e in &errors {
            eprintln!("config: {e}");
        }
        return Err(format!(
            "invalid config ({} error(s)) — fix and retry, or: roster server validate",
            errors.len()
        )
        .into());
    }

    if let Ok(c) = crate::config::snapshot() {
        for w in &c.warnings {
            eprintln!("warning: {w}");
        }
    }

    // One dispatcher per deployment. A second daemon — even on a different
    // --addr, which the port-bind check wouldn't catch — would run its own
    // dispatch loop over the same tasks and double-execute them. Hold this lock
    // for the daemon's whole lifetime; the OS releases it if the process dies.
    let _daemon_lock =
        match crate::statefile::FileLock::try_acquire_path(&crate::paths::lock_file("daemon")) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                return Err(
                    "another roster server is already running for this deployment — \
                        stop it first, or check: roster server status"
                        .into(),
                )
            }
            Err(e) => return Err(format!("could not take the daemon lock: {e}").into()),
        };

    bootstrap_llm_credential().await?;

    // Fetch the box image up front: the daemon tracks the published `:latest`
    // (or `[engine] image`), so every restart picks up the newest engine and
    // the first run never waits on a download mid-dispatch. Absent AND
    // unpullable means no box can run — refuse to boot, loudly.
    if let Ok(c) = crate::config::snapshot() {
        crate::run::boxed::pull_image(&c.box_image).await?;
    }

    let addrs = crate::gateway::resolve_bind_addrs(addr);
    let gateways = crate::gateway::start(&addrs).await.map_err(|e| -> BErr {
        if e.to_string().contains("Address already in use") {
            format!(
                "something is already listening on {} — another roster server? \
                 Check: roster server status  (or pick a different --addr)",
                addrs.join(", ")
            )
            .into()
        } else {
            e
        }
    })?;
    eprintln!(
        "roster server {BUILD} — gateway on {}; dispatch cap {cap}{}{}",
        addrs.join(", "),
        if once { "; once" } else { "" },
        if no_listen { "; listeners off" } else { "" }
    );

    // Channel listeners: one supervised task per worker that declares one
    // ([channels] in its spec).
    let mut listeners = Vec::new();
    if !no_listen && !once {
        let plan = crate::channel::listen::plan();
        if plan.is_empty() {
            eprintln!(
                "listeners: none configured (a worker opts in via [channels] in its worker.toml)"
            );
        }
        for (worker, platform, credential) in plan {
            listeners.push(tokio::spawn(crate::channel::listen::supervised(
                worker, platform, credential,
            )));
        }
        if let Ok(c) = crate::config::snapshot() {
            if let Some(first) = c.workers.first() {
                eprintln!("talk to a worker from another terminal: roster talk {first}");
            }
        }
    }

    // Daily store-snapshot sweep — the catch-all behind the per-run pass
    // (finalize_storage), for stores mutated outside runs or by runs that
    // died before their snapshot. Skipped entirely under --once.
    let sweep = (!once).then(|| {
        tokio::spawn(async {
            loop {
                tokio::time::sleep(Duration::from_secs(24 * 3600)).await;
                let Ok(c) = crate::config::snapshot() else {
                    continue;
                };
                for w in &c.workers {
                    let keep = crate::worker::storage::load(w).store.snapshots;
                    match crate::worker::store::snapshot(w, None, keep) {
                        Ok(Some(o)) => {
                            eprintln!("store sweep {w}: snapshot ({} changes)", o.changes)
                        }
                        Ok(None) => {}
                        Err(e) => eprintln!("store sweep {w}: {e}"),
                    }
                }
            }
        })
    });

    // Dispatch runs in the foreground: with --once it drains due work and
    // returns; otherwise it loops until the process is stopped.
    let dispatch = crate::work::dispatch::dispatch_loop(cap, once);
    tokio::pin!(dispatch);
    let result = tokio::select! {
        r = &mut dispatch => r,
        sig = shutdown_signal() => {
            eprintln!("roster server: {sig} — shutting down");
            // In-flight boxes heard the same signal and are killing their
            // containers; give those handlers a beat to finish their run logs.
            // (dispatch is pinned, not dropped, so its spawned runs live on.)
            tokio::time::sleep(Duration::from_secs(2)).await;
            Ok(())
        }
    };
    for g in gateways {
        g.abort();
    }
    for l in listeners {
        l.abort();
    }
    if let Some(s) = sweep {
        s.abort();
    }
    crate::gateway::clear_state();
    result
}

/// Resolves when the process is told to stop (SIGTERM or Ctrl-C). The daemon
/// must listen at the top level: each box run installs its own process-wide
/// signal stream, which permanently replaces the default die-on-signal
/// disposition — without this, a SIGTERM arriving after the first box run
/// finished was silently swallowed (only SIGKILL worked).
async fn shutdown_signal() -> &'static str {
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(s) => s,
        // Could not install: the kernel's default kill-on-SIGTERM still
        // applies, so only Ctrl-C needs handling here.
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
            return "SIGINT";
        }
    };
    tokio::select! {
        _ = term.recv() => "SIGTERM",
        _ = tokio::signal::ctrl_c() => "SIGINT",
    }
}

/// Boxes cannot call a model without an LLM credential in the vault. On an
/// interactive launch with none present, offer to import the host's pi login
/// (never silently — the user confirms per provider) or run the provider
/// login right here; non-interactive launches get the hint and skip.
/// Import-and-own: after import, roster's gateway owns the refresh and pi's
/// copy may go stale — pi re-logs-in when it next needs to.
async fn bootstrap_llm_credential() -> Result<(), BErr> {
    use crate::credential::LLM_PROVIDERS;
    let present: Vec<&str> = LLM_PROVIDERS
        .iter()
        .copied()
        .filter(|n| crate::credential::vault::get_credential(n).is_some())
        .collect();
    if !present.is_empty() {
        // A connection is a grant by default: heal a deployment whose model
        // credential predates model connections (an admin's hand-written
        // grant wins — nothing is scaffolded over it).
        for name in present {
            crate::cli::connections::ensure_model_grant(name)?;
        }
        return Ok(());
    }
    let interactive = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
    if !interactive {
        eprintln!(
            "no LLM credential in the vault — boxes cannot call a model. \
             Connect one: roster connection add anthropic  (or openai-codex)"
        );
        return Ok(());
    }

    // A host pi login is importable — with confirmation, never silently.
    let pi_auth = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".pi/agent/auth.json");
    let pi_logins = std::fs::read_to_string(&pi_auth)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    let mut imported = false;
    if let Some(logins) = pi_logins.as_ref().and_then(|v| v.as_object()) {
        for name in LLM_PROVIDERS {
            let Some(cred) = logins.get(name).filter(|v| v.is_object()) else {
                continue;
            };
            let answer = crate::credential::connect::ask(&format!(
                "found a pi login for {name}; use it for roster? [y/N] "
            ))?;
            if matches!(answer.trim(), "y" | "Y" | "yes") {
                crate::credential::connect::store(name, cred)
                    .map_err(|e| format!("could not store {name}: {e}"))?;
                eprintln!(
                    "imported {name} — roster now owns the token refresh; \
                     pi will re-login when it next needs to"
                );
                crate::cli::connections::ensure_model_grant(name)?;
                imported = true;
            }
        }
    }
    if imported {
        return Ok(());
    }

    // No import — walk through the provider login right here.
    let answer = crate::credential::connect::ask(
        "no LLM credential yet — connect one now? [anthropic / openai-codex / skip] ",
    )?;
    match answer.trim() {
        p @ ("anthropic" | "openai-codex") => {
            crate::credential::connect::run(p)
                .await
                .map_err(|e| format!("connection add {p}: {e}"))?;
            crate::cli::connections::ensure_model_grant(p)?;
        }
        _ => eprintln!("skipped — connect later with: roster connection add <provider>"),
    }
    Ok(())
}

/// `roster server validate` — parse everything, print every error.
pub fn validate() -> Result<(), BErr> {
    match crate::config::load() {
        Ok(c) => {
            println!(
                "config valid: {} worker(s) [{}], {} grant(s), {} action(s), {} trust rule(s), {} limit(s), {} heartbeat(s), {} listener(s), {} exposure(s)",
                c.workers.len(),
                c.workers.join(", "),
                c.policy.rules.len(),
                c.actions.actions.len(),
                c.actions.trust.len(),
                c.budget.limits.len(),
                c.heartbeats.len(),
                c.listeners.len(),
                c.exposes.len(),
            );
            match &c.engine_dir {
                Some(dir) if !dir.join("box").is_dir() => {
                    println!(
                        "warning: [engine] dir {} has no box/ — sessions will fail",
                        dir.display()
                    )
                }
                Some(dir) => println!(
                    "engine: dev override {} (mounted over the baked engine)",
                    dir.display()
                ),
                None => println!("engine: baked into the box image ({})", c.box_image),
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

/// What a /healthz probe on the recorded port found.
pub enum GatewayHealth {
    Down,
    /// This deployment's daemon answers.
    Up,
    /// Something answers, but it serves another deployment (its config root).
    Foreign(String),
}

/// Probe the gateway on the port the daemon recorded (default :7300) and
/// verify it is OURS via the deployment identity in /healthz — a port is
/// shared machine-wide; the config root is not.
pub async fn gateway_health() -> (u16, GatewayHealth) {
    let port = crate::gateway::recorded_port();
    let resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/healthz"))
        .timeout(Duration::from_millis(700))
        .send()
        .await;
    let health = match resp {
        Ok(r) if r.status().is_success() => {
            let v: serde_json::Value = r.json().await.unwrap_or_default();
            match v.get("config_root").and_then(|s| s.as_str()) {
                Some(root) if root == crate::paths::config_root().display().to_string() => {
                    GatewayHealth::Up
                }
                Some(root) => GatewayHealth::Foreign(root.to_string()),
                // A daemon from before deployment identity: assume ours.
                None => GatewayHealth::Up,
            }
        }
        _ => GatewayHealth::Down,
    };
    (port, health)
}

/// The daemon-liveness probe shared by `server status` and anything that
/// needs OUR server up (talk, chat) — a foreign deployment's daemon on the
/// port counts as down.
pub async fn gateway_up() -> bool {
    matches!(gateway_health().await.1, GatewayHealth::Up)
}

pub async fn status(json: bool) -> Result<(), BErr> {
    let (port, health) = gateway_health().await;
    let gateway_up = matches!(health, GatewayHealth::Up);

    // Config parses? (It loads live — no staleness concept.)
    let config = match crate::config::load() {
        Ok(c) => format!("valid ({} worker(s))", c.workers.len()),
        Err(errors) => format!(
            "INVALID — {} error(s); run: roster server validate",
            errors.len()
        ),
    };

    let mut queue_by_state: BTreeMap<String, usize> = BTreeMap::new();
    for t in tms::list_all() {
        *queue_by_state.entry(t.state).or_insert(0) += 1;
    }
    let gates_pending = gate::list_pending().len();
    let listeners = crate::channel::listen::active_listeners();

    if json {
        let out = serde_json::json!({
            "build": BUILD,
            "gateway": {
                "port": port,
                "up": gateway_up,
                "foreign_deployment": match &health {
                    GatewayHealth::Foreign(root) => Some(root.clone()),
                    _ => None,
                },
            },
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
        match &health {
            GatewayHealth::Up => format!("up on :{port}"),
            GatewayHealth::Down => format!("DOWN (nothing on :{port}) — run: roster server start"),
            GatewayHealth::Foreign(root) => format!(
                "DOWN for this deployment — :{port} is serving another deployment \
                 (config {root}); start this one on a free port: roster server start --addr 127.0.0.1:<port>"
            ),
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
    println!(
        "queue      {queue_line}{}",
        if !gateway_up && queue_by_state.get("pending").copied().unwrap_or(0) > 0 {
            "   (waiting for the server: roster server start)"
        } else {
            ""
        }
    );
    println!(
        "gates      {}",
        if gates_pending == 0 {
            "none pending".to_string()
        } else {
            format!("{gates_pending} PENDING — review: roster server approvals ls")
        }
    );
    if listeners.is_empty() {
        println!("listeners  none");
    } else {
        for (worker, pid, since, alive) in listeners {
            println!(
                "listener   {worker}: {} (pid {pid}, since {since})",
                if alive {
                    "up"
                } else {
                    "STALE LOCK — process gone"
                }
            );
        }
    }
    Ok(())
}
