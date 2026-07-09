//! `roster box` — run one pi session in the locked-down container. Port of the
//! TS box runner + lockdown. The box gets: the repo read-only, a writable
//! workspace/session/HOME, a SENTINEL credential (never the real key), an
//! un-spoofable identity token as proxy creds, and a NAT-disabled network whose
//! only exit is the gateway. Nothing beyond the ceiling timeout.

use crate::util::{now_ms, root};
use base64::Engine;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

type BErr = Box<dyn std::error::Error>;

const LOCKDOWN_NETWORK: &str = "roster-locked";
const GATEWAY_PORT: u16 = 7300;
const BOX_CA_PATH: &str = "/opt/roster/ca.crt";
const SENTINEL: &str = "roster-sentinel-no-real-credential-in-box";

pub async fn run(args: &[String]) -> Result<(), BErr> {
    let mut worker = "adhoc".to_string();
    let mut ceiling_min: f64 = 30.0;
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--worker" => {
                worker = args.get(i + 1).cloned().ok_or("--worker wants a name")?;
                i += 2;
            }
            "--ceiling" => {
                ceiling_min = args.get(i + 1).and_then(|s| s.parse().ok()).ok_or("--ceiling wants a positive number of minutes")?;
                if ceiling_min <= 0.0 {
                    return Err("--ceiling wants a positive number of minutes".into());
                }
                i += 2;
            }
            _ => {
                rest.push(args[i].clone());
                i += 1;
            }
        }
    }
    let prompt = rest.join(" ");
    if prompt.trim().is_empty() {
        return Err("box needs a prompt: roster box \"<prompt>\"".into());
    }

    let (run_id, run_dir, ended_by, exit_code) = run_box(&prompt, ceiling_min, &worker).await?;
    println!("box {run_id} ended by {ended_by} (exit code {})", exit_code.map(|c| c.to_string()).unwrap_or_else(|| "none".into()));
    println!("outputs: {}", run_dir.display());
    std::process::exit(if ended_by == "ceiling" { 2 } else { exit_code.unwrap_or(1) });
}

async fn run_box(prompt: &str, ceiling_min: f64, worker: &str) -> Result<(String, PathBuf, &'static str, Option<i32>), BErr> {
    ensure_lockdown().await?;

    let home = home_dir();
    let host_ca = home.join(".roster/ca/ca.crt");
    if !host_ca.exists() {
        return Err(format!("the gateway CA is not present at {} — start the gateway first (roster serve creates it)", host_ca.display()).into());
    }

    let repo = root();
    let run_id = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
        .chars()
        .take(19)
        .map(|c| if c == 'T' || c == ':' { '-' } else { c })
        .collect::<String>();
    let run_dir = repo.join("runs").join(&run_id);
    let workspace = run_dir.join("workspace");
    let session = run_dir.join("session");
    let pihome = run_dir.join(".pihome");
    std::fs::create_dir_all(&workspace)?;
    std::fs::create_dir_all(&session)?;

    let has_auth = prepare_pihome(&pihome, &home)?;
    if !has_auth && std::env::var("ANTHROPIC_API_KEY").is_err() {
        return Err("no model credentials: neither ~/.pi/agent/auth.json nor ANTHROPIC_API_KEY exists".into());
    }

    // Un-spoofable identity: mint a token, register it off the box mount.
    let subject = format!("org/{worker}");
    let token = uuid::Uuid::new_v4().to_string();
    let identity_dir = home.join(".roster/identity");
    std::fs::create_dir_all(&identity_dir)?;
    let identity_file = identity_dir.join(format!("{token}.json"));
    write_0600(&identity_file, &format!("{}\n", json!({ "subject": subject })))?;

    let proxy_url = format!("http://{token}@host.docker.internal:{GATEWAY_PORT}");
    let container = format!("roster-box-{run_id}");
    let (uid, gid) = (unsafe { libc_getuid() }, unsafe { libc_getgid() });

    let mut args: Vec<String> = vec![
        "run".into(), "--rm".into(), "--name".into(), container.clone(),
        "--add-host=host.docker.internal:host-gateway".into(),
        "--network".into(), LOCKDOWN_NETWORK.into(),
        "-u".into(), format!("{uid}:{gid}"),
        "-v".into(), format!("{0}:{0}:ro", repo.display()),
    ];
    let env_file = repo.join(".env");
    if env_file.exists() {
        args.push("-v".into());
        args.push(format!("/dev/null:{}:ro", env_file.display()));
    }
    let mount = |p: &Path| format!("{0}:{0}", p.display());
    args.extend(["-v".into(), mount(&workspace), "-v".into(), mount(&session), "-v".into(), mount(&pihome)]);
    args.push("-v".into());
    args.push(format!("{}:{BOX_CA_PATH}:ro", host_ca.display()));
    args.extend(["-e".into(), format!("HOME={}", pihome.display())]);
    args.extend(["-e".into(), format!("PI_CODING_AGENT_DIR={}", pihome.join("agent").display())]);
    for k in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
        args.extend(["-e".into(), format!("{k}={proxy_url}")]);
    }
    args.extend(["-e".into(), "NODE_USE_ENV_PROXY=1".into(), "-e".into(), "NO_PROXY=".into()]);
    for k in ["NODE_EXTRA_CA_CERTS", "CURL_CA_BUNDLE", "REQUESTS_CA_BUNDLE"] {
        args.extend(["-e".into(), format!("{k}={BOX_CA_PATH}")]);
    }
    if !has_auth {
        if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
            args.extend(["-e".into(), format!("ANTHROPIC_API_KEY={key}")]);
        }
    }
    args.extend(["-w".into(), workspace.display().to_string(), "roster-box".into()]);
    args.extend(["node".into(), resolve_pi_entry(&repo)?, "--mode".into(), "json".into(), "--no-extensions".into()]);
    args.extend(["--session-dir".into(), session.display().to_string(), prompt.into()]);

    let mut child = tokio::process::Command::new("docker")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;

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
            _ = tokio::signal::ctrl_c(), if !killed => { docker_kill(&container).await; killed = true; }
            _ = sigterm.recv(), if !killed => { docker_kill(&container).await; killed = true; }
        }
    };
    let _ = stream.await;
    let _ = std::fs::remove_file(&identity_file); // single-use token

    Ok((run_id, run_dir, ended_by, status.code()))
}

// ── lockdown ─────────────────────────────────────────────────────────────────

async fn ensure_lockdown() -> Result<(), BErr> {
    let ok = docker_ok(&["network", "inspect", LOCKDOWN_NETWORK])
        || docker_ok(&["network", "create", "-o", "com.docker.network.bridge.enable_ip_masquerade=false", LOCKDOWN_NETWORK]);
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
        return Err(format!("refusing to start the box with open egress: the gateway is not answering on :{GATEWAY_PORT} — start it with: roster serve").into());
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
        let sentinel: Map<String, Value> = real.iter().map(|(k, v)| (k.clone(), sentinelize(v))).collect();
        std::fs::write(agent.join("auth.json"), format!("{}\n", serde_json::to_string_pretty(&sentinel)?))?;
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
    std::fs::write(agent.join("settings.json"), format!("{}\n", serde_json::to_string_pretty(&settings)?))?;
    Ok(has_auth)
}

fn sentinelize(entry: &Value) -> Value {
    let mut e = entry.as_object().cloned().unwrap_or_default();
    if let Some(access) = e.get("access").and_then(|v| v.as_str()) {
        let is_jwt = access.split('.').count() == 3;
        e.insert("access".into(), json!(if is_jwt { sentinel_jwt() } else { SENTINEL.to_string() }));
    }
    if e.get("refresh").and_then(|v| v.as_str()).is_some() {
        e.insert("refresh".into(), json!(SENTINEL));
    }
    if e.get("accountId").and_then(|v| v.as_str()).is_some() {
        e.insert("accountId".into(), json!("roster-sentinel-account"));
    }
    if e.get("expires").and_then(|v| v.as_i64()).is_some() {
        e.insert("expires".into(), json!(now_ms() + 100 * 365 * 24 * 3600 * 1000));
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
