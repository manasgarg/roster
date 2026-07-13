//! `roster worker run` — run one pi session in the locked-down container. Port of the
//! TS box runner + lockdown. The box gets: the repo read-only, a writable
//! workspace/session/HOME, a SENTINEL credential (never the real key), an
//! un-spoofable identity token as proxy creds, and a NAT-disabled network whose
//! only exit is the gateway. Nothing beyond the ceiling timeout.

use crate::util::now_ms;
use base64::Engine;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

type BErr = Box<dyn std::error::Error>;

const LOCKDOWN_NETWORK: &str = "roster-locked";
const GATEWAY_PORT: u16 = 7300;
const BOX_CA_PATH: &str = "/opt/roster/ca.crt";
const BOX_CA_BUNDLE_PATH: &str = "/opt/roster/ca-bundle.crt";
const SENTINEL: &str = "roster-sentinel-no-real-credential-in-box";
const CONTAINER_TEMP: &str = "/tmp:rw,nosuid,nodev,size=2147483648,mode=1777";

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
    crate::worker::memory::save_run_context(&run_id, &run_context)?;
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
        }),
        message: None,
    };
    let compiled = match crate::worker::context::compile_and_trace(&request) {
        Ok(compiled) => compiled,
        Err(error) => {
            crate::run::runlog::fail(&run_id);
            return Err(error.into());
        }
    };
    let (run_id, run_dir, ended_by, exit_code) = match run_box(
        &compiled,
        ceiling_min,
        &worker,
        "",
        &run_id,
        None,
        &run_context,
        "append",
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            crate::run::runlog::fail(&run_id);
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

/// One trusted conversation turn delivered to a warm session. The text goes to
/// the model; the context stays host-side and governs scoped memory actions.
pub struct SessionMessage {
    pub text: String,
    pub author_label: String,
    pub context: crate::worker::memory::RunContext,
}

/// Run one box session for a queued task (the supervisor's entry point). Same
/// machinery as the CLI, but returns the outcome instead of exiting, and passes
/// the task id into the box so proposed actions carry their provenance.
pub async fn dispatch(
    worker: &str,
    task: crate::worker::context::TaskInput,
    run_context: &crate::worker::memory::RunContext,
    ceiling_min: f64,
    task_id: &str,
    run_id: &str,
    code: Option<&CodeSpec>,
    knowledge_mode: &str,
) -> Result<Outcome, BErr> {
    let kind = if knowledge_mode == "reorganization" {
        "reorganization"
    } else if code.is_some() {
        "code"
    } else {
        "task"
    };
    crate::run::runlog::start(run_id, worker, kind, Some(task_id))?;
    let request = crate::worker::context::ContextRequest {
        run_id: run_id.to_string(),
        phase: crate::worker::context::ContextPhase::Start,
        surface: crate::worker::context::RunSurface::QueuedTask,
        worker: worker.to_string(),
        run_context: run_context.clone(),
        task: Some(task),
        message: None,
    };
    let compiled = match crate::worker::context::compile_and_trace(&request) {
        Ok(compiled) => compiled,
        Err(error) => {
            crate::run::runlog::fail(run_id);
            return Err(error.into());
        }
    };
    let (run_id, _run_dir, ended_by, exit_code) = match run_box(
        &compiled,
        ceiling_min,
        worker,
        task_id,
        run_id,
        code,
        run_context,
        knowledge_mode,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            crate::run::runlog::fail(run_id);
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
        &uuid::Uuid::new_v4().simple().to_string()[..4]
    )
}

async fn run_box(
    compiled: &crate::worker::context::CompiledContext,
    ceiling_min: f64,
    worker: &str,
    task_id: &str,
    run_id: &str,
    code: Option<&CodeSpec>,
    run_context: &crate::worker::memory::RunContext,
    knowledge_mode: &str,
) -> Result<(String, PathBuf, &'static str, Option<i32>), BErr> {
    let Provisioned {
        mut args,
        identity_file,
        container,
        session_dir,
        run_dir,
        repo,
        mut storage,
    } = provision_box(
        worker,
        run_id,
        task_id,
        code,
        run_context,
        knowledge_mode,
        Some(ceiling_min),
    )
    .await?;
    args.extend(pi_prefix(&repo, "json", &session_dir)?);
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
            crate::run::runlog::fail(run_id);
            return Err(e.into());
        }
    };

    // Stream the box's stdout to stdout.jsonl.
    let mut out = child.stdout.take().unwrap();
    let stdout_path = run_dir.join("stdout.jsonl");
    let stream = tokio::spawn(async move {
        if let Ok(mut f) = tokio::fs::File::create(&stdout_path).await {
            let _ = tokio::io::copy(&mut out, &mut f).await;
        }
    });

    // Wait, enforcing the ceiling and Ctrl-C / SIGTERM by killing the container.
    let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(ceiling_min * 60.0);
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
    let _ = std::fs::remove_file(&identity_file); // single-use token
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
) -> Result<(), BErr> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    crate::run::runlog::start(run_id, worker, "session", None)?;
    crate::worker::memory::save_run_context(run_id, &start_context)?;
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
            crate::run::runlog::fail(run_id);
            return Err(error.into());
        }
    };

    let Provisioned {
        mut args,
        identity_file,
        container,
        session_dir,
        run_dir,
        repo,
        mut storage,
    } = match provision_box(worker, run_id, "", None, &start_context, "append", None).await {
        Ok(provisioned) => provisioned,
        Err(error) => {
            crate::run::runlog::fail(run_id);
            return Err(error);
        }
    };
    args.insert(1, "-i".into()); // keep stdin open for the rpc protocol
    args.extend(pi_prefix(&repo, "rpc", &session_dir)?);
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
            crate::run::runlog::fail(run_id);
            return Err(e.into());
        }
    };
    let mut stdin = child.stdin.take().ok_or("session box: no stdin")?;
    let stdout = child.stdout.take().ok_or("session box: no stdout")?;
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let mut log = tokio::fs::File::create(run_dir.join("stdout.jsonl"))
        .await
        .ok();

    eprintln!("session {run_id} [{worker}] started");
    let idle = Duration::from_secs(idle_secs);
    let mut rx_open = true;
    let mut busy = false; // a turn is in progress
    let mut clean_exit = false;
    let mut pending: std::collections::VecDeque<SessionMessage> = std::collections::VecDeque::new();
    loop {
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
                    if line.contains("\"type\":\"agent_end\"") {
                        busy = false; // turn complete
                    }
                }
                _ => break, // box closed stdout / exited
            },
            _ = tokio::time::sleep(idle), if !busy => { clean_exit = true; break; }, // idle only when not mid-turn
        }
    }

    drop(stdin);
    docker_kill(&container).await;
    let _ = child.wait().await;
    let _ = std::fs::remove_file(&identity_file); // single-use token
    finalize_storage(&mut storage, clean_exit);
    let _ = crate::run::runlog::finish(
        run_id,
        if clean_exit { "idle" } else { "error" },
        if clean_exit { Some(0) } else { None },
    );
    eprintln!("session {run_id} [{worker}] ended");
    Ok(())
}

// ── shared box provisioning ──────────────────────────────────────────────────

/// The pi command prefix shared by both box modes: node + entry, output mode, no
/// host extension discovery, Roster's own extensions, and the session dir.
fn pi_prefix(repo: &Path, mode: &str, session_dir: &Path) -> Result<Vec<String>, BErr> {
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
    v.extend(["--session-dir".into(), session_dir.display().to_string()]);
    Ok(v)
}

/// Pi maps its session id to provider cache-affinity fields. Per-run session
/// directories keep pi's local transcripts separate even when equivalent runs
/// intentionally share this stable cache route key.
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
struct Provisioned {
    args: Vec<String>,
    identity_file: PathBuf,
    container: String,
    session_dir: PathBuf,
    run_dir: PathBuf,
    repo: PathBuf,
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
    ensure_lockdown().await?;

    let home = home_dir();
    let host_ca = crate::paths::ca_dir().join("ca.crt");
    if !host_ca.exists() {
        return Err(format!("the gateway CA is not present at {} — start the gateway first (roster server start creates it)", host_ca.display()).into());
    }
    // The combined trust bundle (system roots + roster CA) every TLS stack in
    // the box is pointed at. Ensured here too, so a CLI run works even if the
    // daemon predates the bundle.
    let host_bundle = crate::gateway::ca::ensure_bundle().map_err(|e| e.to_string())?;

    // The engine checkout (pi + box extensions) — the ONLY roster-adjacent
    // directory the box mounts; config/data/state live elsewhere entirely.
    let repo = crate::config::snapshot()
        .map_err(|e| format!("config invalid:\n{e}"))?
        .engine_dir
        .clone()
        .ok_or("org.toml needs [engine] dir = \"<path to the roster checkout>\" — the box mounts pi + extensions from there (until they are baked into the box image)")?;
    let run_dir = crate::paths::run_dir(run_id);
    let workspace = run_dir.join("workspace");
    let session = run_dir.join("session");
    let pihome = run_dir.join(".pihome");
    std::fs::create_dir_all(&workspace)?;
    std::fs::create_dir_all(&session)?;
    let storage = crate::worker::knowledge::provision(worker, run_id, knowledge_mode)?;

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
            "no model credentials: neither ~/.pi/agent/auth.json nor ANTHROPIC_API_KEY exists"
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

    let proxy_url = format!("http://{token}@host.docker.internal:{GATEWAY_PORT}");
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
        "-v".into(),
        format!("{0}:{0}:ro", repo.display()),
    ];
    append_container_temp(&mut args);
    let mount = |p: &Path| format!("{0}:{0}", p.display());
    args.extend([
        "-v".into(),
        mount(&workspace),
        "-v".into(),
        mount(&session),
        "-v".into(),
        mount(&pihome),
    ]);
    if let Some(knowledge) = storage.knowledge.as_ref() {
        args.extend([
            "-v".into(),
            format!(
                "{}:{}",
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
    if let Some(wt) = &worktree {
        args.extend(["-v".into(), mount(wt)]);
    }
    args.push("-v".into());
    args.push(format!("{}:{BOX_CA_PATH}:ro", host_ca.display()));
    args.push("-v".into());
    args.push(format!("{}:{BOX_CA_BUNDLE_PATH}:ro", host_bundle.display()));
    args.extend(["-e".into(), format!("HOME={}", pihome.display())]);
    args.extend([
        "-e".into(),
        format!("PI_CODING_AGENT_DIR={}", pihome.join("agent").display()),
    ]);
    args.extend(["-e".into(), format!("ROSTER_RUN_ID={run_id}")]);
    args.extend(["-e".into(), "TMPDIR=/tmp".into()]);
    // Sessions have no wall clock — only task runs get a ceiling to pace against.
    if let Some(min) = ceiling_min {
        args.extend(["-e".into(), format!("ROSTER_CEILING_MIN={min}")]);
    }
    if let Some(knowledge) = storage.knowledge.as_ref() {
        args.extend([
            "-e".into(),
            format!("ROSTER_KNOWLEDGE_DIR={}", knowledge.knowledge_mount()),
            "-e".into(),
            format!("ROSTER_KNOWLEDGE_BASE={}", knowledge.base_commit),
            "-e".into(),
            format!("ROSTER_RECORD_NAMESPACE={}", knowledge.record_namespace),
            "-e".into(),
            format!("ROSTER_KNOWLEDGE_MODE={}", knowledge.mode.as_str()),
        ]);
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
    let cwd = worktree.as_ref().unwrap_or(&workspace);
    args.extend(["-w".into(), cwd.display().to_string(), "roster-box".into()]);

    Ok(Provisioned {
        args,
        identity_file,
        container,
        session_dir: session,
        run_dir,
        repo,
        storage,
    })
}

fn finalize_storage(storage: &mut crate::worker::knowledge::RunStorage, clean: bool) {
    if let Some(checkout) = storage.knowledge.as_ref() {
        if clean && checkout.knowledge_policy.checkpoint_on_clean_exit {
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

// ── lockdown ─────────────────────────────────────────────────────────────────

async fn ensure_lockdown() -> Result<(), BErr> {
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
    let healthy = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{GATEWAY_PORT}/healthz"))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);
    if !healthy {
        return Err(format!("refusing to start the box with open egress: the gateway is not answering on :{GATEWAY_PORT} — start it with: roster server start").into());
    }
    Ok(())
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
    let _ = tokio::process::Command::new("docker")
        .args(["kill", container])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

// ── pihome (sentinel auth) ───────────────────────────────────────────────────

fn prepare_pihome(pihome: &Path, home: &Path) -> Result<bool, BErr> {
    let agent = pihome.join("agent");
    std::fs::create_dir_all(&agent)?;
    let auth_src = home.join(".pi/agent/auth.json");
    let has_auth = auth_src.exists();
    if has_auth {
        let real: Map<String, Value> = serde_json::from_str(&std::fs::read_to_string(&auth_src)?)?;
        let sentinel: Map<String, Value> = real
            .iter()
            .map(|(k, v)| (k.clone(), sentinelize(v)))
            .collect();
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
}
