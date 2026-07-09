//! `roster connect <provider>` — create a credential via the provider's login
//! flow (our own implementation). Generalized over the provider registry
//! (providers.json): api-key providers take a key; OAuth providers run
//! device-code or PKCE. The result lands in the vault; refresh keeps it alive.

use crate::util::{now_ms, root};
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

type BErr = Box<dyn std::error::Error>;

pub async fn run(args: &[String]) -> Result<(), BErr> {
    let name = args.first().ok_or("connect needs a provider: roster connect <provider>")?;
    let registry = read_registry()?;
    let p = registry.get(name).ok_or_else(|| format!("unknown provider \"{name}\" (not in providers.json)"))?;

    let cred = match p.get("auth").and_then(|v| v.as_str()) {
        Some("api_key") => connect_api_key()?,
        Some("oauth") => {
            let login = p.get("login").cloned().unwrap_or_else(|| json!({}));
            match login.get("flow").and_then(|v| v.as_str()) {
                Some("device_code") => connect_device_code(p, &login).await?,
                Some("pkce") => connect_pkce(p, &login).await?,
                other => return Err(format!("provider \"{name}\": unknown oauth flow {other:?}").into()),
            }
        }
        other => return Err(format!("provider \"{name}\": unknown auth {other:?}").into()),
    };

    write_vault(name, &cred)?;
    println!("\nconnected: credential for \"{name}\" written to the vault");
    Ok(())
}

// ── api-key ──────────────────────────────────────────────────────────────────

fn connect_api_key() -> Result<Value, BErr> {
    let key = prompt("paste the API key: ")?;
    if key.is_empty() {
        return Err("no key entered".into());
    }
    Ok(json!({ "type": "api_key", "key": key }))
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
    let device_auth_id = start["device_auth_id"].as_str().ok_or_else(|| format!("device-code start failed: {start}"))?;
    let user_code = start["user_code"].as_str().ok_or("device-code start had no user_code")?;
    let mut wait = start["interval"].as_u64().unwrap_or(5);

    println!("\n  1. open: {}", login["verification_url"].as_str().unwrap_or(""));
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
            if let (Some(c), Some(v)) = (j["authorization_code"].as_str(), j["code_verifier"].as_str()) {
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
    let state = b64url(&random_bytes(16));

    let mut url = reqwest::Url::parse(str_field(login, "authorize_url")?)?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", login["scope"].as_str().unwrap_or(""))
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state);
    println!("\n  open this URL in a browser and authorize:\n\n  {url}\n");

    let (code, got_state) = match login["callback_port"].as_u64() {
        Some(port) => capture_callback(port as u16).or_else(|_| prompt_callback())?,
        None => prompt_callback()?,
    };
    if code.is_empty() {
        return Err("no authorization code captured".into());
    }
    if !got_state.is_empty() && got_state != state {
        return Err("state mismatch — aborting".into());
    }

    let tok = post_json(
        str_field(p, "token_url")?,
        &json!({
            "grant_type": "authorization_code", "client_id": client_id, "code": code, "state": state,
            "redirect_uri": redirect_uri, "code_verifier": verifier,
        }),
    )
    .await?;
    oauth_cred(p, &tok)
}

/// Capture the OAuth redirect on a one-shot local server (raw TCP, no deps).
fn capture_callback(port: u16) -> Result<(String, String), BErr> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", port))?;
    let (mut stream, _) = listener.accept()?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    let path = line.split_whitespace().nth(1).unwrap_or("/");
    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<h3>Roster: you can close this tab.</h3>");
    parse_callback(&format!("http://localhost{path}"))
}

fn prompt_callback() -> Result<(String, String), BErr> {
    let pasted = prompt("paste the redirect URL (or the code) you land on: ")?;
    if let Ok(cs) = parse_callback(&pasted) {
        if !cs.0.is_empty() {
            return Ok(cs);
        }
    }
    Ok((pasted, String::new())) // treat the paste as a bare code
}

fn parse_callback(value: &str) -> Result<(String, String), BErr> {
    let url = reqwest::Url::parse(value)?;
    let mut code = String::new();
    let mut state = String::new();
    for (k, v) in url.query_pairs() {
        if k == "code" {
            code = v.into_owned();
        } else if k == "state" {
            state = v.into_owned();
        }
    }
    Ok((code, state))
}

// ── shared ───────────────────────────────────────────────────────────────────

fn oauth_cred(p: &Value, tok: &Value) -> Result<Value, BErr> {
    let access = tok["access_token"].as_str().ok_or_else(|| format!("token response missing access_token: {tok}"))?;
    let refresh = tok["refresh_token"].as_str().ok_or("token response missing refresh_token")?;
    let expires_in = tok["expires_in"].as_f64().ok_or("token response missing expires_in")?;
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
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).ok()?;
    let mut node: Value = serde_json::from_slice(&bytes).ok()?;
    for key in path {
        node = node.get(*key)?.clone();
    }
    node.as_str().map(String::from)
}

fn read_registry() -> Result<serde_json::Map<String, Value>, BErr> {
    let path = root().join("providers.json");
    let text = std::fs::read_to_string(&path).map_err(|_| format!("no provider registry at {}", path.display()))?;
    Ok(serde_json::from_str::<Value>(&text)?.as_object().cloned().ok_or("providers.json is not an object")?)
}

fn write_vault(name: &str, cred: &Value) -> Result<(), BErr> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let dir = Path::new(&home).join(".roster/vault");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{name}.json"));
    std::fs::write(&path, format!("{}\n", serde_json::to_string_pretty(cred)?))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
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
    v[key].as_str().ok_or_else(|| format!("provider config missing \"{key}\"").into())
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
