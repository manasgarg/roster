//! Provider login flows (our own implementation), generalized over the
//! provider registry: api-key providers take a key; OAuth providers run
//! device-code or PKCE. The result lands in the vault; refresh keeps it
//! alive. Driven by `roster connection add` (and the launch bootstrap) —
//! there is no user-facing "credential" verb (docs/connections.md).

use crate::util::now_ms;
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::time::{Duration, Instant};

type BErr = Box<dyn std::error::Error>;

/// Internal login-and-store, used by the launch bootstrap (LLM providers).
/// The user-facing path is `roster connection add`.
pub async fn run(name: &str) -> Result<(), BErr> {
    let registry = read_registry()?;
    let mut available: Vec<String> = registry.keys().cloned().collect();
    available.sort();
    let p = registry.get(name).ok_or_else(|| {
        format!(
            "unknown provider \"{name}\" — try one of: {}",
            available.join(", ")
        )
    })?;
    let cred = login(name, p, true).await?;
    store(name, &cred)?;
    println!("\nconnected: credential for \"{name}\" written to the vault");
    Ok(())
}

/// Run a provider's interactive login flow and return the credential JSON
/// (not stored). The connections wizard stores it under its own name.
/// `channel_use` widens field collection where a use needs more (slack's
/// Socket Mode app token exists only for the channel listener).
pub async fn login(provider_name: &str, p: &Value, channel_use: bool) -> Result<Value, BErr> {
    // "auth" is a string, or a list of offered methods whose first entry is
    // the default (the connections wizard resolves --auth before calling).
    let auth = match p.get("auth") {
        Some(Value::String(s)) => Some(s.as_str()),
        Some(Value::Array(l)) => l.first().and_then(Value::as_str),
        _ => None,
    };
    match auth {
        Some("api_key") => connect_api_key(),
        Some("smtp") => connect_smtp(),
        Some("discord") => connect_discord(),
        Some("slack") => connect_slack(channel_use),
        Some("oauth") => {
            let login = p.get("login").cloned().unwrap_or_else(|| json!({}));
            match login.get("flow").and_then(|v| v.as_str()) {
                Some("device_code") => connect_device_code(p, &login).await,
                Some("pkce") => connect_pkce(p, &login).await,
                other => Err(
                    format!("provider \"{provider_name}\": unknown oauth flow {other:?}").into(),
                ),
            }
        }
        other => Err(format!("provider \"{provider_name}\": unknown auth {other:?}").into()),
    }
}

/// Store a credential in the vault under `name` (0600, atomic).
pub fn store(name: &str, cred: &Value) -> Result<(), BErr> {
    write_vault(name, cred)
}

/// One line of input from the terminal — the wizard's worker prompt.
pub fn ask(question: &str) -> Result<String, BErr> {
    prompt(question)
}

// ── api-key ──────────────────────────────────────────────────────────────────

fn connect_api_key() -> Result<Value, BErr> {
    let key = prompt_hidden("paste the API key: ")?;
    if key.is_empty() {
        return Err("no key entered".into());
    }
    Ok(json!({ "type": "api_key", "key": key }))
}

// ── SMTP (e.g. Mailgun) ───────────────────────────────────────────────────────

/// Collect SMTP settings for the email executor. Port 465 uses implicit TLS;
/// 587 (or 2525) uses STARTTLS. AUTH LOGIN either way. The credential lands in
/// the vault, off the box; only the trusted-side executor reads it. For Mailgun:
/// host smtp.mailgun.org, the SMTP login (postmaster@your-domain) and its
/// password from the domain's SMTP credentials.
fn connect_smtp() -> Result<Value, BErr> {
    let host = prompt_default("SMTP host [smtp.mailgun.org]: ", "smtp.mailgun.org")?;
    let port: u16 = prompt_default("SMTP port [465 = TLS, 587 = STARTTLS] [465]: ", "465")?
        .parse()
        .map_err(|_| "port must be a number")?;
    let user = prompt("SMTP username (e.g. postmaster@mg.example.com): ")?;
    if user.is_empty() {
        return Err("no username entered".into());
    }
    let pass = prompt_hidden("SMTP password: ")?;
    if pass.is_empty() {
        return Err("no password entered".into());
    }
    let from = prompt_default(&format!("From address [{user}]: "), &user)?;
    Ok(
        json!({ "type": "smtp", "host": host, "port": port, "user": user, "pass": pass, "from": from }),
    )
}

// ── Discord (bot) ─────────────────────────────────────────────────────────────

/// Collect a Discord bot token (for outbound messages) and, optionally, the
/// owner's user id (so message_user can DM them). The token lands in the vault,
/// off the box; only the trusted-side executor reads it.
fn connect_discord() -> Result<Value, BErr> {
    let token = prompt_hidden("Discord bot token: ")?;
    if token.is_empty() {
        return Err("no token entered".into());
    }
    let owner_id = prompt("Owner Discord user id (for DMs; optional): ")?;
    Ok(json!({ "type": "discord", "token": token, "owner_id": owner_id }))
}

/// Slack credential: the bot token does the talking (Web API and the
/// `Bearer {bot_token}` capability injection); the app-level token opens the
/// Socket Mode websocket, so it is collected only when the channel use wants
/// it. Both stay host-side.
fn connect_slack(channel_use: bool) -> Result<Value, BErr> {
    let bot_token = prompt_hidden("Slack bot token (xoxb-…): ")?;
    if bot_token.is_empty() {
        return Err("no bot token entered".into());
    }
    if !bot_token.starts_with("xoxb-") {
        eprintln!("note: bot tokens usually start with xoxb-");
    }
    let mut cred = json!({ "type": "slack", "bot_token": bot_token });
    if channel_use {
        let app_token = prompt_hidden("Slack app-level token (xapp-…, scope connections:write): ")?;
        if app_token.is_empty() {
            return Err("no app-level token entered — Socket Mode needs one (app settings → Basic Information → App-Level Tokens)".into());
        }
        let owner_id = prompt("Your lead's Slack member id (for DMs; optional): ")?;
        cred["app_token"] = json!(app_token);
        cred["owner_id"] = json!(owner_id);
    }
    Ok(cred)
}

// ── OAuth device-code ────────────────────────────────────────────────────────

async fn connect_device_code(p: &Value, login: &Value) -> Result<Value, BErr> {
    let client_id = p["client_id"].as_str().ok_or("provider has no client_id")?;
    let http = reqwest::Client::new();

    let start: Value = http
        .post(str_field(login, "device_authorization_url")?)
        .json(&json!({ "client_id": client_id }))
        .send()
        .await?
        .json()
        .await?;
    let device_auth_id = start["device_auth_id"]
        .as_str()
        .ok_or_else(|| format!("device-code start failed: {start}"))?;
    let user_code = start["user_code"]
        .as_str()
        .ok_or("device-code start had no user_code")?;
    let mut wait = start["interval"].as_u64().unwrap_or(5);

    println!(
        "\n  1. open: {}",
        login["verification_url"].as_str().unwrap_or("")
    );
    println!("  2. enter code: {user_code}\n");
    println!("waiting for you to authorize…");

    let deadline = Instant::now() + Duration::from_secs(15 * 60);
    let (auth_code, verifier) = loop {
        if Instant::now() >= deadline {
            return Err("device-code login timed out".into());
        }
        tokio::time::sleep(Duration::from_secs(wait)).await;
        let res = http
            .post(str_field(login, "device_token_url")?)
            .json(&json!({ "device_auth_id": device_auth_id, "user_code": user_code }))
            .send()
            .await?;
        let status = res.status().as_u16();
        if res.status().is_success() {
            let j: Value = res.json().await?;
            if let (Some(c), Some(v)) = (
                j["authorization_code"].as_str(),
                j["code_verifier"].as_str(),
            ) {
                break (c.to_string(), v.to_string());
            }
        } else if status == 403 || status == 404 {
            continue;
        } else {
            let text = res.text().await.unwrap_or_default();
            match error_code(&text).as_deref() {
                Some("deviceauth_authorization_pending") => continue,
                Some("slow_down") => {
                    wait += 2;
                    continue;
                }
                _ => return Err(format!("device-code poll failed ({status}): {text}").into()),
            }
        }
    };

    let tok = post_form(
        str_field(p, "token_url")?,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("code", &auth_code),
            ("code_verifier", &verifier),
            ("redirect_uri", str_field(login, "exchange_redirect_uri")?),
        ],
    )
    .await?;
    oauth_cred(p, &tok)
}

// ── OAuth PKCE ───────────────────────────────────────────────────────────────

async fn connect_pkce(p: &Value, login: &Value) -> Result<Value, BErr> {
    let client_id = p["client_id"].as_str().ok_or("provider has no client_id")?;
    let redirect_uri = str_field(login, "redirect_uri")?;
    let verifier = b64url(&random_bytes(32));
    let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
    // Some providers (Anthropic) use the verifier as the state; default random.
    let sent_state = if login["state_source"].as_str() == Some("verifier") {
        verifier.clone()
    } else {
        b64url(&random_bytes(16))
    };

    let mut url = reqwest::Url::parse(str_field(login, "authorize_url")?)?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code");
        q.append_pair("client_id", client_id);
        q.append_pair("redirect_uri", redirect_uri);
        q.append_pair("scope", login["scope"].as_str().unwrap_or(""));
        q.append_pair("code_challenge", &challenge);
        q.append_pair("code_challenge_method", "S256");
        q.append_pair("state", &sent_state);
        // Provider-specific extras (e.g. Anthropic's `code=true`).
        if let Some(extra) = login["extra_authorize_params"].as_object() {
            for (k, v) in extra {
                if let Some(s) = v.as_str() {
                    q.append_pair(k, s);
                }
            }
        }
    }
    println!("\n  1. open this URL and authorize:\n\n  {url}\n");
    println!("  2. after authorizing, paste the code (or the full redirect URL) here:\n");

    let (code, got_state) = parse_callback(&prompt("code / redirect URL: ")?);
    if code.is_empty() {
        return Err("no authorization code found in what you pasted".into());
    }
    if !got_state.is_empty() && got_state != sent_state {
        return Err("state mismatch — aborting".into());
    }
    let exchange_state = if got_state.is_empty() {
        sent_state
    } else {
        got_state
    };

    let tok = post_json(
        str_field(p, "token_url")?,
        &json!({
            "grant_type": "authorization_code", "client_id": client_id, "code": code, "state": exchange_state,
            "redirect_uri": redirect_uri, "code_verifier": verifier,
        }),
    )
    .await?;
    oauth_cred(p, &tok)
}

/// Extract (code, state) from what the user pastes: a full redirect URL, a
/// `code#state` string, or a bare code.
fn parse_callback(value: &str) -> (String, String) {
    let value = value.trim();
    if let Ok(url) = reqwest::Url::parse(value) {
        let mut code = String::new();
        let mut state = String::new();
        for (k, v) in url.query_pairs() {
            if k == "code" {
                code = v.into_owned();
            } else if k == "state" {
                state = v.into_owned();
            }
        }
        if !code.is_empty() {
            return (code, state);
        }
    }
    if let Some((code, state)) = value.split_once('#') {
        return (code.to_string(), state.to_string());
    }
    (value.to_string(), String::new())
}

// ── shared ───────────────────────────────────────────────────────────────────

fn oauth_cred(p: &Value, tok: &Value) -> Result<Value, BErr> {
    let access = tok["access_token"]
        .as_str()
        .ok_or_else(|| format!("token response missing access_token: {tok}"))?;
    let refresh = tok["refresh_token"]
        .as_str()
        .ok_or("token response missing refresh_token")?;
    let expires_in = tok["expires_in"]
        .as_f64()
        .ok_or("token response missing expires_in")?;
    let skew = p["skew_ms"].as_i64().unwrap_or(0);
    let mut cred = json!({
        "type": "oauth",
        "access": access,
        "refresh": refresh,
        "expires": now_ms() + (expires_in as i64) * 1000 - skew,
    });
    if let Some(path) = p["account_id_claim"].as_array() {
        let claim: Vec<&str> = path.iter().filter_map(|v| v.as_str()).collect();
        if let Some(id) = jwt_claim(access, &claim) {
            cred["accountId"] = json!(id);
        }
    }
    Ok(cred)
}

fn jwt_claim(jwt: &str, path: &[&str]) -> Option<String> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let mut node: Value = serde_json::from_slice(&bytes).ok()?;
    for key in path {
        node = node.get(*key)?.clone();
    }
    node.as_str().map(String::from)
}

fn read_registry() -> Result<serde_json::Map<String, Value>, BErr> {
    Ok(crate::credential::registry::registry_json())
}

fn write_vault(name: &str, cred: &Value) -> Result<(), BErr> {
    // Same dir logic the gateway reads from (honors ROSTER_VAULT_DIR), so a
    // connected credential always lands where injection/executors look for it.
    let dir = crate::credential::vault::vault_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{name}.json"));
    // Write a temp file, tighten its mode to 0600, THEN rename over the target,
    // so the secret never exists at the umask default (0644) and a crash can't
    // leave a half-written credential — matching vault::write_credential.
    let tmp = dir.join(format!("{name}.json.tmp"));
    std::fs::write(&tmp, format!("{}\n", serde_json::to_string_pretty(cred)?))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

async fn post_json(url: &str, body: &Value) -> Result<Value, BErr> {
    let res = reqwest::Client::new().post(url).json(body).send().await?;
    let status = res.status();
    let text = res.text().await?;
    if !status.is_success() {
        return Err(format!("POST {url} → {status}: {}", &text[..text.len().min(300)]).into());
    }
    Ok(serde_json::from_str(&text)?)
}

async fn post_form(url: &str, form: &[(&str, &str)]) -> Result<Value, BErr> {
    let res = reqwest::Client::new().post(url).form(form).send().await?;
    let status = res.status();
    let text = res.text().await?;
    if !status.is_success() {
        return Err(format!("POST {url} → {status}: {}", &text[..text.len().min(300)]).into());
    }
    Ok(serde_json::from_str(&text)?)
}

fn str_field<'a>(v: &'a Value, key: &str) -> Result<&'a str, BErr> {
    v[key]
        .as_str()
        .ok_or_else(|| format!("provider config missing \"{key}\"").into())
}

fn error_code(text: &str) -> Option<String> {
    let j: Value = serde_json::from_str(text).ok()?;
    match &j["error"] {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => o.get("code").and_then(|v| v.as_str()).map(String::from),
        _ => None,
    }
}

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    buf
}

fn prompt(question: &str) -> Result<String, BErr> {
    print!("{question}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/// Prompt with a fallback used when the reply is empty.
fn prompt_default(question: &str, default: &str) -> Result<String, BErr> {
    let v = prompt(question)?;
    Ok(if v.is_empty() { default.to_string() } else { v })
}

/// Prompt for a secret without echoing it. Turns off terminal echo with `stty`
/// for the duration of the read (best-effort: if stdin isn't a tty or `stty`
/// is unavailable, it falls back to a normal read). Echo is always restored,
/// even if the read fails.
fn prompt_hidden(question: &str) -> Result<String, BErr> {
    print!("{question}");
    std::io::stdout().flush()?;
    let echo_off = set_echo(false);
    let mut line = String::new();
    let result = std::io::stdin().read_line(&mut line);
    if echo_off {
        set_echo(true);
        println!(); // the Enter the terminal didn't echo
    }
    result?;
    Ok(line.trim().to_string())
}

fn set_echo(on: bool) -> bool {
    std::process::Command::new("stty")
        .arg(if on { "echo" } else { "-echo" })
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
