//! `roster worker run` — run one pi session in the locked-down container. Port of the
//! TS box runner + lockdown. The box gets: the repo read-only, a writable
//! workspace/session/HOME, a SENTINEL credential (never the real key), an
//! un-spoofable identity token as proxy creds, and a NAT-disabled network whose
//! only exit is the gateway. Nothing beyond the ceiling timeout.

use crate::util::now_ms;
use base64::Engine as _;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

type BErr = Box<dyn std::error::Error>;

const LOCKDOWN_NETWORK: &str = "roster-locked";
const BOX_CA_PATH: &str = "/opt/roster/ca.crt";
const TASKS_MOUNT: &str = "/opt/roster/tasks.json";
const BOX_CA_BUNDLE_PATH: &str = "/opt/roster/ca-bundle.crt";
const SENTINEL: &str = "roster-sentinel-no-real-credential-in-box";
const CONTAINER_TEMP: &str = "/tmp:rw,nosuid,nodev,size=2147483648,mode=1777";

/// Minutes → a wall-clock `Duration`, never panicking. `Duration::from_secs_f64`
/// aborts on a negative, NaN, infinite, or out-of-range value, and the ceiling
/// once reached this point after the container was already spawned — orphaning a
/// live box. The TMS validates queued ceilings, so this is the backstop: clamp a
/// nonsensical value to a 1-minute floor rather than crash.
fn ceiling_duration(minutes: f64) -> Duration {
    let secs = (minutes.max(0.0) * 60.0).clamp(1.0, (u64::MAX / 2) as f64);
    Duration::from_secs_f64(secs)
}

// Writable run storage is bind-mounted at these fixed CONTAINER paths — never at
// the identical host path — so nothing inside the box (pwd, $HOME, a stack
// trace) reveals the host layout, and the box can't name a real host path to
// plant a file at (F4, defense in depth for the identity boundary). Read-only
// mounts (channel history, CA, the dev engine) keep their paths: they can't be
// used to smuggle a host-visible writable file.
const WORKSPACE_MOUNT: &str = "/workspace";
const SESSION_MOUNT: &str = "/session";
const PIHOME_MOUNT: &str = "/pihome";
const WORKTREE_MOUNT: &str = "/worktree";
// The custom iptables chain that pins the box network's host egress to the
// gateway port when `[box] egress_lockdown` is on (F3).
const EGRESS_CHAIN: &str = "ROSTER-LOCKED";

pub async fn run_once(worker: &str, ceiling_min: f64, prompt: String) -> Result<(), BErr> {
    if ceiling_min <= 0.0 {
        return Err("--ceiling wants a positive number of minutes".into());
    }
    if prompt.trim().is_empty() {
        return Err("worker run needs a prompt".into());
    }
    crate::worker::require_worker(worker)?;
    let worker = worker.to_string();

    let run_id = new_run_id();
    let run_context = crate::worker::memory::RunContext::default();
    crate::run::runlog::start(&run_id, &worker, "box", None)?;
    if let Err(error) = crate::worker::memory::save_run_context(&run_id, &run_context) {
        crate::run::runlog::fail(&run_id, Some(&error));
        return Err(error.into());
    }
    let request = crate::worker::context::ContextRequest {
        run_id: run_id.clone(),
        phase: crate::worker::context::ContextPhase::Start,
        surface: crate::worker::context::RunSurface::DirectBox,
        worker: worker.clone(),
        run_context: run_context.clone(),
        task: Some(crate::worker::context::TaskInput {
            task_id: None,
            origin: "direct".into(),
            text: prompt,
            continuation: None,
            reply_to: None,
        }),
        message: None,
    };
    let compiled = match crate::worker::context::compile_and_trace(&request) {
        Ok(compiled) => compiled,
        Err(error) => {
            crate::run::runlog::fail(&run_id, Some(&error.to_string()));
            return Err(error.into());
        }
    };
    let spec = RunSpec {
        worker: &worker,
        run_id: &run_id,
        task_id: "",
        ceiling_min,
        code: None,
        run_context: &run_context,
        knowledge_mode: "append",
    };
    let (run_id, run_dir, ended_by, exit_code) = match run_box(&compiled, &spec).await {
        Ok(outcome) => outcome,
        Err(error) => {
            crate::run::runlog::fail(&run_id, Some(&error.to_string()));
            return Err(error);
        }
    };
    println!(
        "box {run_id} ended by {ended_by} (exit code {})",
        exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "none".into())
    );
    println!("outputs: {}", run_dir.display());
    std::process::exit(if ended_by == "ceiling" {
        2
    } else {
        exit_code.unwrap_or(1)
    });
}

/// The outcome of one box run, for the supervisor.
pub struct Outcome {
    #[allow(dead_code)] // claim stamps the run id; kept for future consumers
    pub run_id: String,
    pub ended_by: &'static str,
    pub exit_code: Option<i32>,
}

/// A code task's working copy: a fresh git worktree of `repo` at `base`, mounted
/// writable so the box can edit and the git-pr executor can commit + push.
pub struct CodeSpec {
    pub repo: String,
    pub base: String,
}

/// Where pi + the box extensions come from.
enum Engine {
    /// Baked into the roster-box image at /opt/roster/engine (the default).
    /// `run-pi` in the image expands the baked extensions itself, so the host
    /// never inspects image contents.
    Baked,
    /// `[engine] dir` in org.toml — a dev checkout mounted read-only over the
    /// baked engine; entry and extensions resolve from the host filesystem.
    Mounted(PathBuf),
}

/// One trusted conversation turn delivered to a warm session. The text goes to
/// the model; the context stays host-side and governs scoped memory actions.
pub struct SessionMessage {
    pub text: String,
    pub author_label: String,
    pub context: crate::worker::memory::RunContext,
}

/// Everything one boxed run needs to know about itself. Bundled because the
/// dispatch → run → provision chain threads the same facts all the way down.
pub struct RunSpec<'a> {
    pub worker: &'a str,
    pub run_id: &'a str,
    /// Empty for runs with no queued task behind them.
    pub task_id: &'a str,
    pub ceiling_min: f64,
    pub code: Option<&'a CodeSpec>,
    pub run_context: &'a crate::worker::memory::RunContext,
    pub knowledge_mode: &'a str,
}

/// Run one box session for a queued task (the supervisor's entry point). Same
/// machinery as the CLI, but returns the outcome instead of exiting, and passes
/// the task id into the box so proposed actions carry their provenance.
pub async fn dispatch(
    spec: RunSpec<'_>,
    task: crate::worker::context::TaskInput,
) -> Result<Outcome, BErr> {
    let kind = if spec.knowledge_mode == "reorganization" {
        "reorganization"
    } else if spec.code.is_some() {
        "code"
    } else {
        "task"
    };
    crate::run::runlog::start(spec.run_id, spec.worker, kind, Some(spec.task_id))?;
    let request = crate::worker::context::ContextRequest {
        run_id: spec.run_id.to_string(),
        phase: crate::worker::context::ContextPhase::Start,
        surface: crate::worker::context::RunSurface::QueuedTask,
        worker: spec.worker.to_string(),
        run_context: spec.run_context.clone(),
        task: Some(task),
        message: None,
    };
    let compiled = match crate::worker::context::compile_and_trace(&request) {
        Ok(compiled) => compiled,
        Err(error) => {
            crate::run::runlog::fail(spec.run_id, Some(&error.to_string()));
            return Err(error.into());
        }
    };
    let (run_id, _run_dir, ended_by, exit_code) = match run_box(&compiled, &spec).await {
        Ok(outcome) => outcome,
        Err(error) => {
            crate::run::runlog::fail(spec.run_id, Some(&error.to_string()));
            return Err(error);
        }
    };
    Ok(Outcome {
        run_id,
        ended_by,
        exit_code,
    })
}

/// The container name for a run — the supervisor checks `docker ps` for this to
/// tell whether a task marked `running` still has a live box.
pub fn container_name(run_id: &str) -> String {
    format!("roster-box-{run_id}")
}

/// The port the daemon's gateway actually bound (from its state file) — the
/// well-known default when no daemon has recorded one. Boxes, egress rules,
/// and liveness probes all follow the daemon instead of assuming :7300.
fn gateway_port() -> u16 {
    crate::gateway::recorded_port()
}

/// Can a box authenticate to a model right now? The vault first (roster-owned
/// logins), then the host fallbacks provision_box honors. The dispatch loop
/// holds the queue on false instead of burning every task to a failed run.
pub fn model_credentials_available() -> bool {
    crate::credential::LLM_PROVIDERS
        .iter()
        .any(|n| crate::credential::vault::get_credential(n).is_some())
        || home_dir().join(".pi/agent/auth.json").exists()
        || std::env::var("ANTHROPIC_API_KEY").is_ok()
}

/// Is the box container for this run still alive? (For reclaim/requeue safety.)
pub fn box_alive(run_id: &str) -> bool {
    std::process::Command::new("docker")
        .args([
            "ps",
            "-q",
            "--filter",
            &format!("name={}", container_name(run_id)),
        ])
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false)
}

/// Wait for an orphaned box — one a previous supervisor claimed and left running
/// across a restart — to exit, and report its outcome. This lets the new
/// supervisor finalize the task once the box finishes, instead of leaving it
/// wedged in `claimed` (never attested) until the next restart requeues and
/// re-runs work the box already did. We can't reconstruct *why* it ended, only
/// its exit code, so `ended_by` is "exit".
pub async fn adopt(run_id: &str) -> Result<Outcome, String> {
    let container = container_name(run_id);
    let out = tokio::process::Command::new("docker")
        .args(["wait", &container])
        .output()
        .await
        .map_err(|e| format!("docker wait {container}: {e}"))?;
    let exit_code = String::from_utf8_lossy(&out.stdout).trim().parse::<i32>().ok();
    Ok(Outcome {
        run_id: run_id.to_string(),
        ended_by: "exit",
        exit_code,
    })
}

/// A fresh run id: a second-granularity timestamp plus a random suffix so two
/// boxes started in the same second never collide.
pub fn new_run_id() -> String {
    let stamp = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
        .chars()
        .take(19)
        .map(|c| if c == 'T' || c == ':' { '-' } else { c })
        .collect::<String>();
    format!(
        "{stamp}-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    )
}

async fn run_box(
    compiled: &crate::worker::context::CompiledContext,
    spec: &RunSpec<'_>,
) -> Result<(String, PathBuf, &'static str, Option<i32>), BErr> {
    let (worker, run_id, task_id, ceiling_min) =
        (spec.worker, spec.run_id, spec.task_id, spec.ceiling_min);
    let Provisioned {
        mut args,
        identity: _identity, // held for its Drop: removes the token on any exit
        container,
        session_dir,
        run_dir,
        engine,
        mut storage,
    } = provision_box(
        worker,
        run_id,
        task_id,
        spec.code,
        spec.run_context,
        spec.knowledge_mode,
        Some(ceiling_min),
    )
    .await?;
    args.extend(pi_prefix(&engine, "json", &session_dir)?);
    append_cache_session_id(&mut args, &compiled.cache.route_key);
    if !compiled.system_prompt.is_empty() {
        args.extend([
            "--append-system-prompt".into(),
            compiled.system_prompt.clone(),
        ]);
    }
    args.push(
        compiled
            .input_prompt
            .clone()
            .ok_or("one-shot context has no input prompt")?,
    );

    let mut child = match tokio::process::Command::new("docker")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            crate::run::runlog::fail(run_id, Some(&e.to_string()));
            return Err(e.into());
        }
    };

    // Stream the box's stdout to stdout.jsonl.
    let mut out = child.stdout.take().unwrap();
    let stdout_path = run_dir.join("stdout.jsonl");
    let stream = tokio::spawn(async move {
        match tokio::fs::File::create(&stdout_path).await {
            Ok(mut f) => {
                let _ = tokio::io::copy(&mut out, &mut f).await;
            }
            Err(e) => {
                // Don't just drop `out`: that shuts the pipe and the box may die
                // with EPIPE mid-run, then be reported as a failure with no
                // transcript explaining why. Log, and drain so the box runs on.
                eprintln!(
                    "run: could not open {} ({e}); transcript will be missing",
                    stdout_path.display()
                );
                let _ = tokio::io::copy(&mut out, &mut tokio::io::sink()).await;
            }
        }
    });

    // Wait, enforcing the ceiling and Ctrl-C / SIGTERM by killing the container.
    let deadline = tokio::time::Instant::now() + ceiling_duration(ceiling_min);
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut ended_by = "exit";
    let mut killed = false;
    let status = loop {
        tokio::select! {
            s = child.wait() => break s?,
            _ = tokio::time::sleep_until(deadline), if !killed => { docker_kill(&container).await; ended_by = "ceiling"; killed = true; }
            _ = tokio::signal::ctrl_c(), if !killed => { docker_kill(&container).await; ended_by = "signal"; killed = true; }
            _ = sigterm.recv(), if !killed => { docker_kill(&container).await; ended_by = "signal"; killed = true; }
        }
    };
    let _ = stream.await;
    // _identity drops at function end and removes the single-use token.
    finalize_storage(&mut storage, ended_by == "exit" && status.success());
    let _ = crate::run::runlog::finish(run_id, ended_by, status.code());

    Ok((run_id.to_string(), run_dir, ended_by, status.code()))
}

// ── rpc session box (warm, multi-message) ────────────────────────────────────

/// Run a persistent box in pi's rpc mode: one warm container that handles a
/// stream of messages (delivered on `rx`) in a single pi session, exiting after
/// `idle_secs` of silence or when the box dies. Reuses provision_box, so the
/// lockdown/identity are identical to a one-shot run. The context compiler
/// creates the stable system prefix once and a volatile input for each turn.
pub async fn run_session(
    worker: &str,
    run_id: &str,
    surface: crate::worker::context::RunSurface,
    start_context: crate::worker::memory::RunContext,
    mut rx: tokio::sync::mpsc::Receiver<SessionMessage>,
    idle_secs: u64,
    reply_tx: Option<tokio::sync::mpsc::Sender<String>>,
) -> Result<(), BErr> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    crate::run::runlog::start(run_id, worker, "session", None)?;
    if let Err(error) = crate::worker::memory::save_run_context(run_id, &start_context) {
        crate::run::runlog::fail(run_id, Some(&error));
        return Err(error.into());
    }
    let start_request = crate::worker::context::ContextRequest {
        run_id: run_id.to_string(),
        phase: crate::worker::context::ContextPhase::Start,
        surface: surface.clone(),
        worker: worker.to_string(),
        run_context: start_context.clone(),
        task: None,
        message: None,
    };
    let start = match crate::worker::context::compile_and_trace(&start_request) {
        Ok(compiled) => compiled,
        Err(error) => {
            crate::run::runlog::fail(run_id, Some(&error.to_string()));
            return Err(error.into());
        }
    };

    let Provisioned {
        mut args,
        identity: _identity, // held for its Drop: removes the token on any exit
        container,
        session_dir,
        run_dir,
        engine,
        mut storage,
    } = match provision_box(worker, run_id, "", None, &start_context, "append", None).await {
        Ok(provisioned) => provisioned,
        Err(error) => {
            crate::run::runlog::fail(run_id, Some(&error.to_string()));
            return Err(error);
        }
    };
    // Warm-session wall-clock bounds (F5): the idle timer only fires between
    // turns, so a turn that never ends (a tool loop, a wedged agent) would run
    // forever. A per-turn ceiling and a whole-session ceiling backstop it.
    let box_policy = crate::config::snapshot()
        .map(|c| c.box_policy.clone())
        .unwrap_or_default();
    let session_ceiling = ceiling_duration(box_policy.session_ceiling_min);
    let turn_ceiling = ceiling_duration(box_policy.turn_ceiling_min);

    args.insert(1, "-i".into()); // keep stdin open for the rpc protocol
    match pi_prefix(&engine, "rpc", &session_dir) {
        Ok(prefix) => args.extend(prefix),
        Err(error) => {
            crate::run::runlog::fail(run_id, Some(&error.to_string()));
            return Err(error);
        }
    }
    append_cache_session_id(&mut args, &start.cache.route_key);
    args.extend(["--append-system-prompt".into(), start.system_prompt]);

    let mut child = match tokio::process::Command::new("docker")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            crate::run::runlog::fail(run_id, Some(&e.to_string()));
            return Err(e.into());
        }
    };
    let mut stdin = child.stdin.take().ok_or("session box: no stdin")?;
    let stdout = child.stdout.take().ok_or("session box: no stdout")?;
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let mut log = tokio::fs::File::create(run_dir.join("stdout.jsonl"))
        .await
        .ok();

    // On the terminal surface stdout/stderr ARE the conversation — lifecycle
    // lines would land after the "you> " prompt, so talk announces the
    // session itself and this stays quiet.
    let announce = !matches!(surface, crate::worker::context::RunSurface::TermSession);
    if announce {
        eprintln!("session {run_id} [{worker}] started");
    }
    let idle = Duration::from_secs(idle_secs);
    let session_deadline = tokio::time::Instant::now() + session_ceiling;
    let mut turn_started: Option<tokio::time::Instant> = None;
    let mut rx_open = true;
    let mut busy = false; // a turn is in progress
    let mut clean_exit = false;
    let mut pending: std::collections::VecDeque<SessionMessage> = std::collections::VecDeque::new();
    loop {
        // The sender is gone (stdin EOF, listener shutdown) and nothing is in
        // flight: end the session now instead of sitting out the idle window.
        if !rx_open && !busy && pending.is_empty() {
            clean_exit = true;
            break;
        }
        // Serialize: feed the next message only once the previous turn is done, so
        // rapid messages become distinct turns (in order) rather than coalescing.
        if !busy {
            if let Some(msg) = pending.pop_front() {
                if let Err(error) = crate::worker::memory::save_run_context(run_id, &msg.context) {
                    eprintln!(
                        "session {run_id}: context save failed; message not delivered: {error}"
                    );
                    continue;
                }
                let request = crate::worker::context::ContextRequest {
                    run_id: run_id.to_string(),
                    phase: crate::worker::context::ContextPhase::Turn,
                    surface: surface.clone(),
                    worker: worker.to_string(),
                    run_context: msg.context.clone(),
                    task: None,
                    message: Some(crate::worker::context::MessageInput {
                        provider: msg.context.provider.clone(),
                        message_id: msg.context.message_id.clone(),
                        author_label: msg.author_label,
                        role: msg.context.role.clone(),
                        text: msg.text,
                    }),
                };
                let compiled = match crate::worker::context::compile_and_trace(&request) {
                    Ok(compiled) => compiled,
                    Err(error) => {
                        eprintln!("session {run_id}: context compilation failed; message not delivered: {error}");
                        continue;
                    }
                };
                let line = json!({
                    "type": "prompt",
                    "message": compiled.input_prompt.unwrap_or_default()
                })
                .to_string();
                if stdin.write_all(line.as_bytes()).await.is_err()
                    || stdin.write_all(b"\n").await.is_err()
                {
                    break;
                }
                let _ = stdin.flush().await;
                busy = true;
                turn_started = Some(tokio::time::Instant::now());
            }
        }
        tokio::select! {
            m = rx.recv(), if rx_open => match m {
                Some(msg) => pending.push_back(msg),
                None => rx_open = false, // sender dropped; drain, then idle out
            },
            l = lines.next_line() => match l {
                Ok(Some(line)) => {
                    if let Some(f) = log.as_mut() {
                        let _ = f.write_all(line.as_bytes()).await;
                        let _ = f.write_all(b"\n").await;
                    }
                    if let Some(tx) = reply_tx.as_ref() {
                        for text in assistant_text(&line) {
                            let _ = tx.send(text).await;
                        }
                    }
                    if line.contains("\"type\":\"agent_end\"") {
                        busy = false; // turn complete
                        turn_started = None;
                    }
                }
                _ => break, // box closed stdout / exited
            },
            _ = tokio::time::sleep(idle), if !busy => { clean_exit = true; break; }, // idle only when not mid-turn
            _ = tokio::time::sleep_until(session_deadline) => {
                eprintln!("session {run_id}: reached the session ceiling ({:.0}m); ending", box_policy.session_ceiling_min);
                clean_exit = !busy; // a clean bound between turns; an error if it cuts a turn
                break;
            }
            _ = sleep_until_opt(turn_started.map(|t| t + turn_ceiling)), if busy => {
                eprintln!("session {run_id}: a turn ran past the turn ceiling ({:.0}m); ending", box_policy.turn_ceiling_min);
                clean_exit = false;
                break;
            }
        }
    }

    drop(stdin);
    docker_kill(&container).await;
    let _ = child.wait().await;
    // _identity drops at function end and removes the single-use token.
    finalize_storage(&mut storage, clean_exit);
    let _ = crate::run::runlog::finish(
        run_id,
        if clean_exit { "idle" } else { "error" },
        if clean_exit { Some(0) } else { None },
    );
    if announce {
        eprintln!("session {run_id} [{worker}] ended");
    }
    // A clean idle wind-down (or a clean bound between turns) is success; the box
    // dying mid-turn or a stdin write failure is not — report it so the caller
    // can tell the user rather than claim the conversation just "wound down".
    if clean_exit {
        Ok(())
    } else {
        Err("the session box exited before completing the turn".into())
    }
}

// ── shared box provisioning ──────────────────────────────────────────────────

/// The pi command prefix shared by both box modes: node + entry, output mode, no
/// host extension discovery, Roster's own extensions, and the session dir.
fn pi_prefix(engine: &Engine, mode: &str, session_dir: &Path) -> Result<Vec<String>, BErr> {
    let mut v = match engine {
        // The wrapper supplies --no-extensions and the baked extension list.
        Engine::Baked => vec![
            "/opt/roster/engine/run-pi".into(),
            "--mode".into(),
            mode.into(),
        ],
        Engine::Mounted(repo) => {
            let mut v = vec![
                "node".into(),
                resolve_pi_entry(repo)?,
                "--mode".into(),
                mode.into(),
                "--no-extensions".into(),
            ];
            for ext in box_extensions(repo) {
                v.push("-e".into());
                v.push(ext);
            }
            v
        }
    };
    v.extend(["--session-dir".into(), session_dir.display().to_string()]);
    Ok(v)
}

/// Pi maps its session id to provider cache-affinity fields. Per-run session
/// directories keep pi's local transcripts separate even when equivalent runs
/// intentionally share this stable cache route key.
/// The assistant's reply text in a pi rpc `message_end` event, if this line is
/// one. A turn may end several assistant messages; only those with text parts
/// (the words meant for the human) yield output — thinking and tool calls don't.
fn assistant_text(line: &str) -> Vec<String> {
    if !line.contains("\"type\":\"message_end\"") {
        return Vec::new();
    }
    let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
        return Vec::new();
    };
    if event["type"] != "message_end" || event["message"]["role"] != "assistant" {
        return Vec::new();
    }
    let Some(parts) = event["message"]["content"].as_array() else {
        return Vec::new();
    };
    parts
        .iter()
        .filter(|p| p["type"] == "text")
        .filter_map(|p| p["text"].as_str())
        .map(str::to_string)
        .filter(|t| !t.trim().is_empty())
        .collect()
}

fn append_cache_session_id(args: &mut Vec<String>, route_key: &str) {
    if !route_key.is_empty() {
        args.extend(["--session-id".into(), route_key.into()]);
    }
}

/// Everything a box needs, up to (but not including) the pi command: the lockdown
/// check, per-run dirs, an optional code worktree, the sentinel pihome, a minted
/// identity token, and the docker args through the image + cwd. Shared by the
/// one-shot runner and the rpc session runner so the lockdown/identity setup is
/// defined exactly once.
/// The minted per-run identity token file. Removed on drop, so any error or
/// panic between provisioning and the box's exit still cleans up the token
/// rather than leaving a valid gateway credential on disk.
struct IdentityToken {
    path: PathBuf,
}

impl Drop for IdentityToken {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

struct Provisioned {
    args: Vec<String>,
    identity: IdentityToken,
    container: String,
    session_dir: PathBuf,
    run_dir: PathBuf,
    engine: Engine,
    storage: crate::worker::knowledge::RunStorage,
}

async fn provision_box(
    worker: &str,
    run_id: &str,
    task_id: &str,
    code: Option<&CodeSpec>,
    run_context: &crate::worker::memory::RunContext,
    knowledge_mode: &str,
    ceiling_min: Option<f64>,
) -> Result<Provisioned, BErr> {
    let config = crate::config::snapshot().map_err(|e| format!("config invalid:\n{e}"))?;
    ensure_lockdown(&config.box_policy).await?;

    let home = home_dir();
    let host_ca = crate::paths::ca_dir().join("ca.crt");
    if !host_ca.exists() {
        return Err(format!("the gateway CA is not present at {} — start the gateway first (roster server start creates it)", host_ca.display()).into());
    }
    // The combined trust bundle (system roots + roster CA) every TLS stack in
    // the box is pointed at. Ensured here too, so a CLI run works even if the
    // daemon predates the bundle.
    let host_bundle = crate::gateway::ca::ensure_bundle().map_err(|e| e.to_string())?;

    // The engine: baked into the image by default; `[engine] dir` (a dev
    // checkout, the ONLY roster-adjacent directory the box ever mounts) wins
    // when set. Config/data/state live elsewhere entirely.
    let engine = match config.engine_dir.clone() {
        Some(dir) => Engine::Mounted(dir),
        None => Engine::Baked,
    };
    let run_dir = crate::paths::run_dir(run_id);
    let workspace = run_dir.join("workspace");
    let session = run_dir.join("session");
    let pihome = run_dir.join(".pihome");
    std::fs::create_dir_all(&workspace)?;
    std::fs::create_dir_all(&session)?;
    let storage =
        crate::worker::knowledge::provision(worker, run_id, knowledge_mode, run_context.tainted())?;

    // Code task: a writable git worktree on a fresh per-run branch.
    let worktree: Option<PathBuf> = match code {
        Some(cs) => {
            let wt = run_dir.join("worktree");
            let branch = format!("worker/{worker}/{run_id}");
            let ok = std::process::Command::new("git")
                .args(["-C", &cs.repo, "worktree", "add", "-B", &branch])
                .arg(&wt)
                .arg(&cs.base)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !ok {
                return Err(
                    format!("could not create worktree from {} at {}", cs.repo, cs.base).into(),
                );
            }
            Some(wt)
        }
        None => None,
    };

    let has_auth = prepare_pihome(&pihome, &home)?;
    if !has_auth && std::env::var("ANTHROPIC_API_KEY").is_err() {
        return Err(
            "no model credential — connect one: roster connection add anthropic  (or openai-codex). \
             Also honored: a host pi login (~/.pi/agent/auth.json) or ANTHROPIC_API_KEY in the daemon's environment"
                .into(),
        );
    }

    // Un-spoofable identity: mint a token, register it off the box mount.
    let subject = format!("org/{worker}");
    let token = uuid::Uuid::new_v4().to_string();
    let identity_dir = crate::paths::identity_dir();
    std::fs::create_dir_all(&identity_dir)?;
    let identity_file = identity_dir.join(format!("{token}.json"));
    write_0600(
        &identity_file,
        &format!("{}\n", json!({ "subject": subject, "run_id": run_id })),
    )?;

    let proxy_url = format!("http://{token}@host.docker.internal:{}", gateway_port());
    let container = container_name(run_id);
    let (uid, gid) = (unsafe { libc_getuid() }, unsafe { libc_getgid() });

    let mut args: Vec<String> = vec![
        "run".into(),
        "--rm".into(),
        "--name".into(),
        container.clone(),
        "--add-host=host.docker.internal:host-gateway".into(),
        "--network".into(),
        LOCKDOWN_NETWORK.into(),
        // Proxied clients hand hostnames to CONNECT — the box needs no DNS.
        // A blackhole resolver closes the DNS-exfiltration side door Docker's
        // embedded resolver would otherwise leave open (it forwards via the
        // host daemon). host.docker.internal is an /etc/hosts entry, unaffected.
        "--dns".into(),
        "127.0.0.1".into(),
        "-u".into(),
        format!("{uid}:{gid}"),
    ];
    // Container hardening (F2). Non-root already blocks the easy cases; these
    // close the rest: drop every capability, forbid setuid privilege escalation
    // (the image still ships su/mount/newgrp…), and bound processes so a rogue
    // or prompt-injected agent can't fork-bomb or memory/CPU-starve the *host*
    // (a shared kernel). Memory/CPU caps apply only when configured.
    let box_policy = &config.box_policy;
    args.extend([
        "--cap-drop".into(),
        "ALL".into(),
        "--security-opt".into(),
        "no-new-privileges".into(),
    ]);
    if box_policy.pids_limit > 0 {
        args.extend(["--pids-limit".into(), box_policy.pids_limit.to_string()]);
    }
    if let Some(mem) = &box_policy.memory {
        args.extend(["--memory".into(), mem.clone()]);
    }
    if let Some(cpus) = &box_policy.cpus {
        args.extend(["--cpus".into(), cpus.clone()]);
    }
    if let Engine::Mounted(repo) = &engine {
        args.extend(["-v".into(), format!("{0}:{0}:ro", repo.display())]);
    }
    append_container_temp(&mut args);
    // Writable run storage at fixed container paths, not identical host paths (F4).
    args.extend([
        "-v".into(),
        format!("{}:{WORKSPACE_MOUNT}", workspace.display()),
        "-v".into(),
        format!("{}:{SESSION_MOUNT}", session.display()),
        "-v".into(),
        format!("{}:{PIHOME_MOUNT}", pihome.display()),
    ]);
    if let Some(knowledge) = storage.knowledge.as_ref() {
        let ro = if knowledge.mode == crate::worker::knowledge::KnowledgeMode::Read {
            ":ro"
        } else {
            ""
        };
        args.extend([
            "-v".into(),
            format!(
                "{}:{}{ro}",
                knowledge.path.display(),
                knowledge.knowledge_mount()
            ),
        ]);
    }
    if let Some(channel) = run_context.channel_id.as_deref() {
        let channel_dir = crate::paths::channel_dir(channel);
        if channel_dir.is_dir() {
            args.extend(["-v".into(), format!("{0}:{0}:ro", channel_dir.display())]);
        }
    }
    // The worker's task partition as a live read view (docs/work.md):
    // the host rewrites it in place on every mutation;
    // authoritative writes go through the set_tasks action, file edits are
    // scratch.
    let tasks_view = crate::work::tms::ensure_view(worker);
    args.extend([
        "-v".into(),
        format!("{}:{TASKS_MOUNT}", tasks_view.display()),
    ]);
    if let Some(wt) = &worktree {
        args.extend(["-v".into(), format!("{}:{WORKTREE_MOUNT}", wt.display())]);
    }
    args.push("-v".into());
    args.push(format!("{}:{BOX_CA_PATH}:ro", host_ca.display()));
    args.push("-v".into());
    args.push(format!("{}:{BOX_CA_BUNDLE_PATH}:ro", host_bundle.display()));
    args.extend(["-e".into(), format!("HOME={PIHOME_MOUNT}")]);
    args.extend([
        "-e".into(),
        format!("PI_CODING_AGENT_DIR={PIHOME_MOUNT}/agent"),
    ]);
    args.extend(["-e".into(), format!("ROSTER_RUN_ID={run_id}")]);
    args.extend(["-e".into(), format!("ROSTER_TASKS_FILE={TASKS_MOUNT}")]);
    args.extend(["-e".into(), "TMPDIR=/tmp".into()]);
    // Sessions have no wall clock — only task runs get a ceiling to pace against.
    if let Some(min) = ceiling_min {
        args.extend(["-e".into(), format!("ROSTER_CEILING_MIN={min}")]);
    }
    // [[expose]] — credential env vars, set to the SENTINEL. The gateway's
    // per-grant injection swaps in the real value in transit, only where that
    // grant's scope allows; the box env never holds a secret.
    let subject = format!("org/{}", crate::paths::short_worker(worker));
    for e in &config.exposes {
        if crate::gateway::scope::applies(&e.scope, &subject) {
            args.extend(["-e".into(), format!("{}={SENTINEL}", e.env)]);
        }
    }
    if let Some(knowledge) = storage.knowledge.as_ref() {
        args.extend([
            "-e".into(),
            format!("ROSTER_KNOWLEDGE_DIR={}", knowledge.knowledge_mount()),
            "-e".into(),
            format!("ROSTER_KNOWLEDGE_BASE={}", knowledge.base_commit),
            "-e".into(),
            format!("ROSTER_KNOWLEDGE_MODE={}", knowledge.mode.as_str()),
        ]);
        // Read-only checkouts have no write contract, so no namespace.
        if knowledge.mode != crate::worker::knowledge::KnowledgeMode::Read {
            args.extend([
                "-e".into(),
                format!("ROSTER_RECORD_NAMESPACE={}", knowledge.record_namespace),
            ]);
        }
    }
    if !task_id.is_empty() {
        args.extend(["-e".into(), format!("ROSTER_TASK_ID={task_id}")]);
    }
    for k in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
        args.extend(["-e".into(), format!("{k}={proxy_url}")]);
    }
    args.extend([
        "-e".into(),
        "NODE_USE_ENV_PROXY=1".into(),
        "-e".into(),
        "NO_PROXY=".into(),
    ]);
    // Trust for terminated TLS, per ecosystem. NODE_EXTRA_CA_CERTS is additive
    // (Node keeps its built-in roots), so the bare CA suffices; everything else
    // REPLACES default roots and gets the combined bundle — Go and OpenSSL
    // tools (SSL_CERT_FILE), curl, Python requests/pip, git.
    args.extend(["-e".into(), format!("NODE_EXTRA_CA_CERTS={BOX_CA_PATH}")]);
    for k in [
        "SSL_CERT_FILE",
        "CURL_CA_BUNDLE",
        "REQUESTS_CA_BUNDLE",
        "GIT_SSL_CAINFO",
        "PIP_CERT",
    ] {
        args.extend(["-e".into(), format!("{k}={BOX_CA_BUNDLE_PATH}")]);
    }
    if !has_auth {
        if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
            args.extend(["-e".into(), format!("ANTHROPIC_API_KEY={key}")]);
        }
    }
    let cwd = if worktree.is_some() {
        WORKTREE_MOUNT
    } else {
        WORKSPACE_MOUNT
    };
    args.extend(["-w".into(), cwd.into(), "roster-box".into()]);

    Ok(Provisioned {
        args,
        identity: IdentityToken {
            path: identity_file,
        },
        container,
        // The container-side session path pi is pointed at (`--session-dir`);
        // on the host this maps back to `session` (run_dir/session).
        session_dir: PathBuf::from(SESSION_MOUNT),
        run_dir,
        engine,
        storage,
    })
}

fn finalize_storage(storage: &mut crate::worker::knowledge::RunStorage, clean: bool) {
    if let Some(checkout) = storage.knowledge.as_ref() {
        if checkout.mode == crate::worker::knowledge::KnowledgeMode::Read {
            // Consultation only: nothing to integrate, nothing to quarantine.
        } else if clean && checkout.knowledge_policy.checkpoint_on_clean_exit {
            match crate::worker::knowledge::checkpoint(checkout) {
                Ok(result) if result.files > 0 => eprintln!(
                    "knowledge {}: integrated {} file(s) at {}",
                    checkout.run_id,
                    result.files,
                    result.commit.as_deref().unwrap_or("-")
                ),
                Ok(_) => {}
                Err(error) => eprintln!(
                    "knowledge {}: checkpoint rejected: {error}",
                    checkout.run_id
                ),
            }
        } else if clean {
            let _ = crate::run::runlog::update_knowledge(
                &checkout.run_id,
                "uncheckpointed",
                None,
                Some("automatic checkpoint disabled by policy"),
            );
        } else {
            crate::worker::knowledge::quarantine(checkout, "run did not exit cleanly");
        }
    }
    if let Err(error) = crate::worker::knowledge::release_reorganization(storage) {
        eprintln!(
            "knowledge {}: could not journal reorganization lease release: {error}",
            storage.run_id
        );
    }
}

fn append_container_temp(args: &mut Vec<String>) {
    args.extend(["--tmpfs".into(), CONTAINER_TEMP.into()]);
}

/// Sleep until a deadline, or never if there is none — lets the warm-session
/// select arm the per-turn ceiling only while a turn is actually in flight.
async fn sleep_until_opt(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending::<()>().await,
    }
}

// ── lockdown ─────────────────────────────────────────────────────────────────

async fn ensure_lockdown(box_policy: &crate::config::BoxPolicy) -> Result<(), BErr> {
    let ok = docker_ok(&["network", "inspect", LOCKDOWN_NETWORK])
        || docker_ok(&[
            "network",
            "create",
            "-o",
            "com.docker.network.bridge.enable_ip_masquerade=false",
            LOCKDOWN_NETWORK,
        ]);
    if !ok {
        return Err(format!("refusing to start the box with open egress: the \"{LOCKDOWN_NETWORK}\" docker network could not be created").into());
    }
    let port = gateway_port();
    // Masquerade-off breaks the *internet* return path, but the host itself
    // stays reachable on every port (F3). Pin host egress to the gateway when
    // asked; otherwise make the residual reach legible, once per process.
    if box_policy.egress_lockdown {
        ensure_egress_lockdown(port)?;
    } else {
        warn_open_host_egress(port);
    }
    // A loopback-only daemon is healthy from the host but unreachable from a
    // container — refuse with the fix instead of a confusing timeout later.
    if let Some(addrs) = crate::gateway::recorded_addrs() {
        let box_reachable = addrs
            .iter()
            .any(|a| !a.starts_with("127.") && !a.starts_with("localhost"));
        if !box_reachable {
            return Err(format!(
                "the gateway is bound to loopback only ({}) — boxes reach it via \
                 host.docker.internal and can't connect. Restart without --addr (the default \
                 also binds the docker bridge) or with --addr 0.0.0.0:{port}",
                addrs.join(", ")
            )
            .into());
        }
    }
    let health = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/healthz"))
        .timeout(Duration::from_secs(2))
        .send()
        .await;
    let body: Option<serde_json::Value> = match health {
        Ok(r) if r.status().is_success() => r.json().await.ok(),
        _ => {
            return Err(format!("refusing to start the box with open egress: the gateway is not answering on :{port} — start it with: roster server start").into());
        }
    };
    // The right port answering is not enough — it must be THIS deployment's
    // daemon, or the box would run under another deployment's policy.
    if let Some(root) = body
        .as_ref()
        .and_then(|v| v.get("config_root"))
        .and_then(|v| v.as_str())
    {
        let ours = crate::paths::config_root().display().to_string();
        if root != ours {
            return Err(format!(
                "the gateway on :{port} belongs to another deployment (config {root}) — \
                 start this deployment's own server: roster server start"
            )
            .into());
        }
    }
    Ok(())
}

fn warn_open_host_egress(gateway_port: u16) {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        eprintln!(
            "note: the box can't reach the internet, but the host stays reachable on all ports \
             (a local DB, another container's published port, a TCP docker socket, ssh) — these \
             bypass the gateway. Set `[box] egress_lockdown = true` in org.toml to pin box egress \
             to the gateway on :{gateway_port} (needs root / CAP_NET_ADMIN for iptables)."
        );
    });
}

/// Pin the locked network's host egress to the gateway port only. Uses a
/// dedicated iptables chain, rebuilt each run so ordering is deterministic and
/// idempotent without touching any other INPUT rule; a single `-s <subnet>`
/// jump routes box→host traffic into it. Fails closed (refuses to start the
/// box) if the rules can't be installed — matching the rest of the lockdown.
fn ensure_egress_lockdown(gateway_port: u16) -> Result<(), BErr> {
    let subnet = locked_subnet()?;
    let priv_err = || -> BErr {
        format!(
            "[box] egress_lockdown is on but the iptables rules could not be installed for {subnet} \
             (need root / CAP_NET_ADMIN). Run the daemon with the capability, or unset egress_lockdown."
        )
        .into()
    };
    // Our own chain: create if absent, then flush so we own its exact contents.
    let _ = ipt(&["-N", EGRESS_CHAIN]); // ignore "chain already exists"
    if !ipt(&["-F", EGRESS_CHAIN]) {
        return Err(priv_err());
    }
    for rule in egress_chain_rules(gateway_port) {
        let args: Vec<&str> = std::iter::once("-A")
            .chain(std::iter::once(EGRESS_CHAIN))
            .chain(rule.iter().map(String::as_str))
            .collect();
        if !ipt(&args) {
            return Err(priv_err());
        }
    }
    // Route box→host traffic into the chain (idempotent via -C before -I).
    let jump = ["INPUT", "-s", subnet.as_str(), "-j", EGRESS_CHAIN];
    let present = ipt(&[&["-C"], &jump[..]].concat());
    if !present && !ipt(&[&["-I"], &jump[..]].concat()) {
        return Err(priv_err());
    }
    eprintln!("box egress locked: {subnet} may reach the host only on :{gateway_port}");
    Ok(())
}

/// The ordered contents of the egress chain: accept the gateway port, drop the
/// rest. Pure, so the ordering (accept before drop) is unit-tested.
fn egress_chain_rules(gateway_port: u16) -> Vec<Vec<String>> {
    vec![
        vec![
            "-p".into(),
            "tcp".into(),
            "--dport".into(),
            gateway_port.to_string(),
            "-j".into(),
            "ACCEPT".into(),
        ],
        vec!["-j".into(), "DROP".into()],
    ]
}

fn locked_subnet() -> Result<String, BErr> {
    let out = std::process::Command::new("docker")
        .args([
            "network",
            "inspect",
            LOCKDOWN_NETWORK,
            "-f",
            "{{range .IPAM.Config}}{{.Subnet}}{{end}}",
        ])
        .output()?;
    let subnet = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if subnet.is_empty() {
        return Err(
            format!("could not read the {LOCKDOWN_NETWORK} subnet for egress lockdown").into(),
        );
    }
    Ok(subnet)
}

/// Run one `iptables` invocation, quietly; true on success.
fn ipt(args: &[&str]) -> bool {
    std::process::Command::new("iptables")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn docker_ok(args: &[&str]) -> bool {
    std::process::Command::new("docker")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

async fn docker_kill(container: &str) {
    // A failed kill means the ceiling/signal wasn't actually enforced — surface
    // it instead of silently waiting on a box that may run past its deadline.
    match tokio::process::Command::new("docker")
        .args(["kill", container])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
    {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!("run: docker kill {container} exited {s} — box may still be running"),
        Err(e) => eprintln!("run: could not docker kill {container} ({e}) — box may still be running"),
    }
}

// ── pihome (sentinel auth) ───────────────────────────────────────────────────

fn prepare_pihome(pihome: &Path, home: &Path) -> Result<bool, BErr> {
    let agent = pihome.join("agent");
    std::fs::create_dir_all(&agent)?;
    // Sentinel logins for pi, shaped like real ones (pi reads the account id
    // and expiry) but never containing a secret — the gateway injects the real
    // credential in transit. A host pi login seeds the map; vault logins
    // (`roster connection add anthropic`) fill in for hosts that never ran pi.
    let auth_src = home.join(".pi/agent/auth.json");
    let mut sentinel: Map<String, Value> = Map::new();
    if auth_src.exists() {
        let real: Map<String, Value> = serde_json::from_str(&std::fs::read_to_string(&auth_src)?)?;
        sentinel = real
            .iter()
            .map(|(k, v)| (k.clone(), sentinelize(v)))
            .collect();
    }
    for name in crate::credential::LLM_PROVIDERS {
        if sentinel.contains_key(name) {
            continue;
        }
        if let Some(cred) = crate::credential::vault::get_credential(name) {
            sentinel.insert(name.to_string(), sentinelize_known_fields(&Value::Object(cred)));
        }
    }
    let has_auth = !sentinel.is_empty();
    if has_auth {
        std::fs::write(
            agent.join("auth.json"),
            format!("{}\n", serde_json::to_string_pretty(&sentinel)?),
        )?;
    }
    // Rebuild settings: only the model selection carries over.
    let host: Map<String, Value> = std::fs::read_to_string(home.join(".pi/agent/settings.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let mut settings = Map::new();
    for k in ["defaultProvider", "defaultModel", "defaultThinkingLevel"] {
        if let Some(v) = host.get(k) {
            settings.insert(k.to_string(), v.clone());
        }
    }
    std::fs::write(
        agent.join("settings.json"),
        format!("{}\n", serde_json::to_string_pretty(&settings)?),
    )?;
    Ok(has_auth)
}

/// `sentinelize` for a vault credential: mask, then keep ONLY the fields pi
/// reads. A vault entry may carry fields sentinelize doesn't know about (an
/// api key, provider extras) — the whitelist guarantees none of them can ride
/// into the box.
fn sentinelize_known_fields(entry: &Value) -> Value {
    let masked = sentinelize(entry);
    let mut out = Map::new();
    if let Some(o) = masked.as_object() {
        for k in ["type", "access", "refresh", "accountId", "expires", "key"] {
            if let Some(v) = o.get(k) {
                out.insert(k.into(), v.clone());
            }
        }
    }
    Value::Object(out)
}

fn sentinelize(entry: &Value) -> Value {
    let mut e = entry.as_object().cloned().unwrap_or_default();
    if let Some(access) = e.get("access").and_then(|v| v.as_str()) {
        let is_jwt = access.split('.').count() == 3;
        e.insert(
            "access".into(),
            json!(if is_jwt {
                sentinel_jwt()
            } else {
                SENTINEL.to_string()
            }),
        );
    }
    if e.get("refresh").and_then(|v| v.as_str()).is_some() {
        e.insert("refresh".into(), json!(SENTINEL));
    }
    if e.get("key").and_then(|v| v.as_str()).is_some() {
        e.insert("key".into(), json!(SENTINEL));
    }
    if e.get("accountId").and_then(|v| v.as_str()).is_some() {
        e.insert("accountId".into(), json!("roster-sentinel-account"));
    }
    if e.get("expires").and_then(|v| v.as_i64()).is_some() {
        e.insert(
            "expires".into(),
            json!(now_ms() + 100 * 365 * 24 * 3600 * 1000),
        );
    }
    Value::Object(e)
}

/// A structurally-valid but useless JWT so pi can decode it (it reads the
/// account id + expiry) without it being a real credential.
fn sentinel_jwt() -> String {
    let now_sec = now_ms() / 1000;
    let b64 = |v: &Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string());
    let header = b64(&json!({ "alg": "none", "typ": "JWT" }));
    let payload = b64(&json!({
        "iat": now_sec,
        "exp": now_sec + 100 * 365 * 24 * 3600i64,
        "https://api.openai.com/auth": { "chatgpt_account_id": "roster-sentinel-account" },
    }));
    let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("roster-sentinel-signature");
    format!("{header}.{payload}.{sig}")
}

// ── misc ─────────────────────────────────────────────────────────────────────

/// pi's real JS entrypoint (the npm .bin shim is a shell script; read the bin
/// field so the box invokes it with plain node).
fn resolve_pi_entry(repo: &Path) -> Result<String, BErr> {
    let pkg_dir = std::fs::canonicalize(repo.join("node_modules/@earendil-works/pi-coding-agent"))?;
    let pkg: Value = serde_json::from_str(&std::fs::read_to_string(pkg_dir.join("package.json"))?)?;
    let bin = match &pkg["bin"] {
        Value::String(s) => s.clone(),
        b => b["pi"].as_str().ok_or("pi package has no bin")?.to_string(),
    };
    Ok(pkg_dir.join(bin).display().to_string())
}

/// Roster's vendored box extensions: every `.ts` under box/extensions/, sorted.
/// Dropping a new file there is enough to ship a new capability into the box.
fn box_extensions(repo: &Path) -> Vec<String> {
    let dir = repo.join("box/extensions");
    let mut paths: Vec<String> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("ts"))
        .map(|p| p.display().to_string())
        .collect();
    paths.sort();
    paths
}

/// Owner-controlled worker identity path. Prompt assembly lives in the context
/// compiler; this helper remains for the identity admin surfaces.
pub fn identity_path(worker: &str) -> PathBuf {
    crate::paths::worker_dir(worker).join("identity.md")
}

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

fn write_0600(path: &Path, contents: &str) -> Result<(), BErr> {
    std::fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

extern "C" {
    fn getuid() -> u32;
    fn getgid() -> u32;
}
unsafe fn libc_getuid() -> u32 {
    getuid()
}
unsafe fn libc_getgid() -> u32 {
    getgid()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_temp_is_a_bounded_tmpfs() {
        let mut args = Vec::new();
        append_container_temp(&mut args);
        assert_eq!(args, vec!["--tmpfs", CONTAINER_TEMP]);
    }

    #[test]
    fn cache_route_key_becomes_the_pi_session_id() {
        let mut args = vec!["node".into(), "pi".into()];
        append_cache_session_id(&mut args, "roster-pc-abc123");
        assert_eq!(args, vec!["node", "pi", "--session-id", "roster-pc-abc123"]);
    }

    #[test]
    fn egress_chain_accepts_gateway_port_before_dropping() {
        let rules = egress_chain_rules(7300);
        // Order is load-bearing: the gateway-port ACCEPT must precede the DROP,
        // or the box would lose its only exit.
        assert_eq!(
            rules[0],
            vec!["-p", "tcp", "--dport", "7300", "-j", "ACCEPT"]
        );
        assert_eq!(rules[1], vec!["-j", "DROP"]);
    }
}
