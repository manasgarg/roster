//! The proxy core: accept a connection, answer /healthz, and for CONNECT either
//! raw-tunnel (the `tunnel` verdict, for cert-pinning clients) or terminate TLS
//! and judge each decrypted request before forwarding. See docs/gateway.md.

use crate::credential::vault;
use crate::gateway::ca::Ca;
use crate::gateway::judge::judge;
use crate::gateway::schema::{GovernedRequest, Mcp, Policy, Verdict};
use crate::paths;
use crate::util::now_rfc3339;
use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::header::HeaderMap;
use hyper::server::conn::http1 as server_http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;

pub type BErr = Box<dyn std::error::Error + Send + Sync>;
pub type Body = BoxBody<Bytes, BErr>;
pub type UpstreamClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Body>;

const SENSITIVE: [&str; 5] = [
    "authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "proxy-authorization",
];

// ── paths & config ──────────────────────────────────────────────────────────

/// Read the policy fresh each decision so admin edits are live. Fail closed: an
/// unparseable policy denies everything (empty rule list).
fn load_policy() -> Policy {
    match crate::config::snapshot() {
        Ok(c) => c.policy.clone(),
        Err(e) => {
            eprintln!("gateway: INVALID CONFIG — denying all until it parses\n{e}");
            Policy::empty()
        }
    }
}

// ── decision log ────────────────────────────────────────────────────────────

fn next_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}-{n:x}")
}

fn record(
    gr: &GovernedRequest,
    verdict: Verdict,
    rule: Option<&str>,
    injected: Option<&[String]>,
    spend: &std::collections::HashMap<String, f64>,
    note: Option<&str>,
) {
    let headers: serde_json::Map<String, Value> = gr
        .headers
        .iter()
        .map(|(k, v)| {
            let val = if SENSITIVE.contains(&k.as_str()) {
                "<redacted>".to_string()
            } else {
                v.clone()
            };
            (k.clone(), Value::String(val))
        })
        .collect();
    let mcp = match &gr.mcp {
        Some(m) => json!({ "method": m.method, "tool": m.tool }),
        None => Value::Null,
    };
    let mut dec = json!({
        "decision_id": next_id(),
        "ts": now_rfc3339(),
        "verdict": verdict.as_str(),
        "rule": rule,
        "request": {
            "worker": gr.worker,
            "protocol": gr.protocol,
            "method": gr.method,
            "host": gr.host,
            "port": gr.port,
            "path": gr.path,
            "query": gr.query,
            "headers": Value::Object(headers),
            "bodySize": gr.body_size,
            "mcp": mcp,
        },
        "spend": spend,
    });
    if let Some(inj) = injected {
        dec["injected"] = json!(inj);
    }
    if let Some(n) = note {
        dec["note"] = json!(n);
    }

    // Serialize the append so concurrent requests can't interleave into a
    // corrupt line: this is the tamper-evident record the gateway exists to
    // produce, so a garbled entry is worse than a slow one.
    let path = paths::decisions_log();
    match crate::statefile::FileLock::acquire("audit-decisions") {
        Ok(_lock) => {
            if let Err(e) = crate::statefile::append_line(&path, &dec.to_string()) {
                eprintln!("gateway: could not append decision record ({e})");
            }
        }
        Err(e) => eprintln!("gateway: could not lock decision log ({e}); decision not recorded"),
    }
    eprintln!(
        "{} {} {}{} {}",
        verdict.as_str(),
        gr.method,
        gr.host,
        gr.path,
        rule.unwrap_or("(no rule)")
    );
}

// ── request shaping ─────────────────────────────────────────────────────────

fn lower_headers(map: &HeaderMap) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    for (k, v) in map.iter() {
        let key = k.as_str().to_lowercase();
        let val = v.to_str().unwrap_or("").to_string();
        out.entry(key)
            .and_modify(|existing| {
                existing.push_str(", ");
                existing.push_str(&val);
            })
            .or_insert(val);
    }
    out
}

/// Lift MCP's own terms from a JSON-RPC body, if that's what this is.
fn lift_mcp(headers: &HashMap<String, String>, body: &[u8]) -> Option<Mcp> {
    let ct = headers
        .get("content-type")
        .map(|s| s.as_str())
        .unwrap_or("");
    if body.is_empty() || !ct.contains("json") {
        return None;
    }
    let v: Value = serde_json::from_slice(body).ok()?;
    let msg = if v.is_array() { v.get(0)?.clone() } else { v };
    let method = msg.get("method")?.as_str()?.to_string();
    let is_rpc = msg.get("jsonrpc").and_then(|j| j.as_str()) == Some("2.0") || method.contains('/');
    if !is_rpc {
        return None;
    }
    let tool = if method == "tools/call" {
        msg.get("params")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .map(|s| s.to_string())
    } else {
        None
    };
    Some(Mcp { method, tool })
}

// ── body helpers ────────────────────────────────────────────────────────────

fn full(s: &str) -> Body {
    Full::new(Bytes::from(s.to_string()))
        .map_err(|never| match never {})
        .boxed()
}

fn empty() -> Body {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

// ── per-run refusal tally ───────────────────────────────────────────────────

/// Refused calls per run, in this daemon's memory — evidence for task
/// attestation: a run that ends silently after refusals is not a success.
/// Read-and-cleared by dispatch when it finalizes the task; session runs
/// leave tiny entries behind, bounded by runs-per-daemon-lifetime.
static REFUSALS: std::sync::OnceLock<std::sync::Mutex<HashMap<String, u32>>> =
    std::sync::OnceLock::new();

fn refusal_tally() -> &'static std::sync::Mutex<HashMap<String, u32>> {
    REFUSALS.get_or_init(Default::default)
}

fn note_refusal(run_id: &str) {
    if run_id.is_empty() {
        return;
    }
    *refusal_tally()
        .lock()
        .unwrap()
        .entry(run_id.to_string())
        .or_insert(0) += 1;
}

/// How many of this run's calls the gateway refused; clears the tally.
pub fn take_refusals(run_id: &str) -> u32 {
    refusal_tally().lock().unwrap().remove(run_id).unwrap_or(0)
}

// ── identity ────────────────────────────────────────────────────────────────

/// A subject that no grant, action, or budget can ever match — the resolved
/// identity for a box that presents a token that isn't a real minted one. Scope
/// matching is `subject == scope || subject.startsWith("scope/")`, and every
/// real scope is "org" or "org/<worker>", so this leading-'!' subject nests
/// under nothing and default-denies everywhere (invariant #11, fail closed).
const UNRESOLVED_SUBJECT: &str = "!unresolved";

/// Resolve the call's subject and run from the CONNECT's Proxy-Authorization. The
/// trusted runner sets `HTTP(S)_PROXY=http://<token>@…` and registers
/// `<state>/identity/<token>.json = {subject}` (never mounted into the box).
///
/// The token IS box-controlled input — the agent has a shell and can craft any
/// `Proxy-Authorization` it likes — so this must never (a) trust it as identity
/// or (b) let it steer a filesystem path. Two guards:
///   - No proxy credential at all ⇒ a trusted host-side caller (the roster
///     binary itself never proxies through here); resolve to "org".
///   - A box-presented token must be exactly a minted v4 UUID. Anything else, or
///     a well-formed token with no matching registration, resolves to the
///     un-grantable UNRESOLVED_SUBJECT — never to a real subject and never to
///     the fleet root. Validating the UUID *before* the join also closes the
///     path-traversal: no valid UUID contains `/`, `\`, or `.`, so the token
///     can't escape identity_dir or point at a box-written file.
fn resolve_identity(proxy_auth: Option<&hyper::header::HeaderValue>) -> (String, String) {
    let host_side = || ("org".to_string(), String::new());
    let unresolved = || (UNRESOLVED_SUBJECT.to_string(), String::new());
    let Some(b64) = proxy_auth
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Basic "))
    else {
        return host_side();
    };
    use base64::Engine;
    let token = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .ok()
        .and_then(|d| String::from_utf8(d).ok())
        .map(|creds| creds.split(':').next().unwrap_or("").to_string())
        .unwrap_or_default();
    if token.is_empty() {
        // A "Basic :" style empty username is not a host-side caller (those send
        // no Proxy-Authorization at all) — treat it as an unresolved box token.
        return unresolved();
    }
    if uuid::Uuid::parse_str(&token).is_err() {
        return unresolved();
    }
    let path = crate::paths::identity_dir().join(format!("{token}.json"));
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| {
            let subject = v.get("subject")?.as_str()?.to_string();
            let run_id = v
                .get("run_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Some((subject, run_id))
        })
        .unwrap_or_else(unresolved)
}

/// A policy denial a CLI can read: the status line carries the verdict and
/// rule in headers (for clients that print nothing else), the body carries
/// them again plus a hint that this is governance, not an outage.
fn deny_response(verdict: Verdict, rule: Option<&str>) -> Response<Body> {
    let rule_json = rule
        .map(|r| format!("\"{r}\""))
        .unwrap_or_else(|| "null".into());
    let mut resp = Response::new(full(&format!(
        "{{\"error\":\"denied by gateway ({})\",\"rule\":{},\"hint\":\"policy said no — retrying won't change the answer; propose an action or ask your lead\"}}",
        verdict.as_str(),
        rule_json
    )));
    *resp.status_mut() = StatusCode::FORBIDDEN;
    let headers = resp.headers_mut();
    headers.insert("x-roster-verdict", "deny".parse().unwrap());
    if let Some(rule) = rule {
        if let Ok(v) = rule.parse() {
            headers.insert("x-roster-rule", v);
        }
    }
    resp
}

/// The rule allowed this call, but its injected credential is missing or expired.
/// That is an operations problem the admin can fix — not policy — so say so
/// distinctly (503, a different hint) instead of masquerading as a denial that
/// "retrying won't change".
fn credential_outage_response(rule: Option<&str>) -> Response<Body> {
    let rule_json = rule
        .map(|r| format!("\"{r}\""))
        .unwrap_or_else(|| "null".into());
    let mut resp = Response::new(full(&format!(
        "{{\"error\":\"gateway could not supply the credential for this call\",\"rule\":{rule_json},\"hint\":\"the rule allows this, but its credential is missing or expired — this is an outage, not policy; ask your admin to (re)connect it, then retry\"}}"
    )));
    *resp.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    let headers = resp.headers_mut();
    headers.insert("x-roster-verdict", "credential-outage".parse().unwrap());
    if let Some(rule) = rule {
        if let Ok(v) = rule.parse() {
            headers.insert("x-roster-rule", v);
        }
    }
    resp
}

// ── server ──────────────────────────────────────────────────────────────────

pub fn build_client() -> UpstreamClient {
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()
        .expect("load native root certs")
        .https_or_http()
        .enable_http1()
        .build();
    Client::builder(TokioExecutor::new()).build(https)
}

pub async fn serve(stream: TcpStream, tls: TlsAcceptor, client: UpstreamClient, _ca: Arc<Ca>) {
    let io = TokioIo::new(stream);
    let svc = service_fn(move |req| outer(req, tls.clone(), client.clone()));
    if let Err(e) = server_http1::Builder::new()
        .serve_connection(io, svc)
        .with_upgrades()
        .await
    {
        let _ = e;
    }
}

async fn outer(
    req: Request<Incoming>,
    tls: TlsAcceptor,
    client: UpstreamClient,
) -> Result<Response<Body>, BErr> {
    if req.method() == Method::CONNECT {
        let authority = req
            .uri()
            .authority()
            .map(|a| a.to_string())
            .unwrap_or_default();
        let host = authority.split(':').next().unwrap_or("").to_string();
        let port: u16 = authority
            .split(':')
            .nth(1)
            .and_then(|p| p.parse().ok())
            .unwrap_or(443);
        let (subject, run_id) =
            resolve_identity(req.headers().get(hyper::header::PROXY_AUTHORIZATION));

        // Tunnel escape hatch: judge host+port only; if the rule says tunnel,
        // raw-pipe without terminating (host-only visibility).
        let pre = GovernedRequest {
            worker: Some(subject.clone()),
            protocol: "https".into(),
            method: "CONNECT".into(),
            host: host.clone(),
            port,
            path: String::new(),
            query: String::new(),
            headers: HashMap::new(),
            body_size: 0,
            mcp: None,
        };
        let (verdict, rule) = judge(&pre, &load_policy());
        if verdict == Verdict::Tunnel {
            record(
                &pre,
                Verdict::Tunnel,
                rule.as_deref(),
                None,
                &HashMap::new(),
                None,
            );
            tokio::spawn(async move {
                let upgraded = match hyper::upgrade::on(req).await {
                    Ok(u) => u,
                    Err(_) => return,
                };
                let mut client_io = TokioIo::new(upgraded);
                if let Ok(mut upstream) = TcpStream::connect((host.as_str(), port)).await {
                    let _ = tokio::io::copy_bidirectional(&mut client_io, &mut upstream).await;
                }
            });
            return Ok(Response::new(empty()));
        }

        // Otherwise terminate TLS and judge each decrypted request.
        tokio::spawn(async move {
            let upgraded = match hyper::upgrade::on(req).await {
                Ok(u) => u,
                Err(_) => return,
            };
            let tls_stream = match tls.accept(TokioIo::new(upgraded)).await {
                Ok(s) => s,
                Err(_) => return,
            };
            let io = TokioIo::new(tls_stream);
            let svc = service_fn(move |r| {
                handle(
                    r,
                    "https",
                    host.clone(),
                    subject.clone(),
                    run_id.clone(),
                    client.clone(),
                )
            });
            let _ = server_http1::Builder::new()
                .serve_connection(io, svc)
                .with_upgrades()
                .await;
        });
        Ok(Response::new(empty()))
    } else if req.uri().path() == "/healthz" {
        // Deployment identity rides along so a probe can tell OUR daemon from
        // another deployment's daemon squatting on the same port.
        let body = serde_json::json!({
            "ok": true,
            "config_root": crate::paths::config_root().display().to_string(),
        })
        .to_string();
        let mut resp = Response::new(full(&body));
        resp.headers_mut().insert(
            hyper::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        Ok(resp)
    } else if req.uri().scheme_str() == Some("http") {
        let host = req.uri().host().unwrap_or("").to_string();
        let (subject, run_id) =
            resolve_identity(req.headers().get(hyper::header::PROXY_AUTHORIZATION));
        handle(req, "http", host, subject, run_id, client).await
    } else {
        let mut resp = Response::new(full("{\"error\":\"not a proxy request\"}"));
        *resp.status_mut() = StatusCode::BAD_REQUEST;
        Ok(resp)
    }
}

/// The governance decision for a request: the injected headers to apply, or a
/// ready deny response. Judge + inject + budget + record + debit live here once,
/// shared by the HTTP and WebSocket forward paths.
enum Gate {
    Deny(Response<Body>),
    Allow(Vec<(String, String)>),
}

async fn gate(gr: &GovernedRequest, subject: &str) -> Gate {
    let policy = load_policy();
    let (verdict, rule) = judge(gr, &policy);

    // Injection: resolve the rule's credential now (refresh if expired) so we
    // fail closed — deny rather than forward the sentinel — when it's missing.
    let mut inject: Vec<(String, String)> = Vec::new();
    let mut injected_names: Option<Vec<String>> = None;
    if verdict == Verdict::Allow {
        if let Some(rule_name) = &rule {
            if let Some(inj) = policy.rule(rule_name).and_then(|r| r.inject.as_ref()) {
                match vault::get_fresh_credential(&inj.credential).await {
                    Err(_) | Ok(None) => {
                        // The rule matched (Allow); the credential is just absent.
                        // Record it as its own outcome, not a policy denial, so the
                        // audit log and the caller both see a fixable outage.
                        record(
                            gr,
                            Verdict::Allow,
                            rule.as_deref(),
                            None,
                            &HashMap::new(),
                            Some(&format!(
                                "credential outage: \"{}\" missing or expired",
                                inj.credential
                            )),
                        );
                        return Gate::Deny(credential_outage_response(rule.as_deref()));
                    }
                    Ok(Some(cred)) => {
                        inject = if inj.headers.is_empty() {
                            vault::render_injection(
                                &cred,
                                inj.provider.as_deref().unwrap_or(&inj.credential),
                            )
                        } else {
                            vault::render_headers(&cred, &inj.headers)
                        };
                        injected_names = Some(inject.iter().map(|(k, _)| k.clone()).collect());
                    }
                }
            }
        }
    }

    // Meter + enforce the budget against the call's subject (ancestor rollup).
    let budget = crate::gateway::budget::load_budget();
    let now = crate::util::now_ms();
    let spend = if verdict == Verdict::Allow {
        crate::gateway::budget::compute_spend(
            gr,
            verdict.as_str(),
            rule.as_deref(),
            &json!({}),
            &budget,
        )
    } else {
        HashMap::new()
    };
    if verdict == Verdict::Allow {
        if let Some(refusal) = crate::gateway::ledger::check(subject, &spend, &budget.limits, now) {
            record(
                gr,
                Verdict::Deny,
                rule.as_deref(),
                None,
                &HashMap::new(),
                Some(&refusal.reason),
            );
            let mut resp = Response::new(full(&format!(
                "{{\"error\":\"budget exceeded\",\"detail\":\"{}\",\"retry_after_secs\":{},\"hint\":\"a budget window is used up — nothing is broken; retry after it resets\"}}",
                refusal.reason, refusal.retry_after_secs
            )));
            *resp.status_mut() = StatusCode::PAYMENT_REQUIRED;
            let headers = resp.headers_mut();
            headers.insert("x-roster-verdict", "budget".parse().unwrap());
            if let Ok(v) = refusal.retry_after_secs.to_string().parse() {
                headers.insert(hyper::header::RETRY_AFTER, v);
            }
            return Gate::Deny(resp);
        }
    }

    record(
        gr,
        verdict,
        rule.as_deref(),
        injected_names.as_deref(),
        &spend,
        None,
    );
    if verdict != Verdict::Allow {
        return Gate::Deny(deny_response(verdict, rule.as_deref()));
    }
    crate::gateway::ledger::debit(subject, &spend, &budget.limits, now);
    Gate::Allow(inject)
}

/// A decrypted (or plain-http) request: judge, then forward. WebSocket upgrades
/// are tunneled (see forward_websocket); everything else is a buffered forward
/// with the response streamed back.
async fn handle(
    req: Request<Incoming>,
    protocol: &str,
    host: String,
    subject: String,
    run_id: String,
    client: UpstreamClient,
) -> Result<Response<Body>, BErr> {
    // The action host is served internally: parse the envelope and let the
    // action layer attribute, authorize, and execute-or-gate it. Never forwarded.
    if host == crate::action::ACTION_HOST {
        let (parts, incoming) = req.into_parts();
        let method = parts.method.as_str().to_string();
        let path = parts.uri.path().to_string();
        let body = incoming
            .collect()
            .await
            .map(|c| c.to_bytes())
            .unwrap_or_default();
        return Ok(crate::action::handle_action(&subject, &run_id, &method, &path, &body).await);
    }

    let headers = lower_headers(req.headers());
    let is_ws = headers
        .get("upgrade")
        .map(|u| u.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let port: u16 = if protocol == "https" { 443 } else { 80 };

    if is_ws {
        // A WebSocket handshake carries no body; judge on headers, then tunnel.
        let gr = GovernedRequest {
            worker: Some(subject.clone()),
            protocol: protocol.into(),
            method,
            host: host.clone(),
            port,
            path,
            query,
            headers,
            body_size: 0,
            mcp: None,
        };
        return match gate(&gr, &subject).await {
            Gate::Deny(resp) => {
                note_refusal(&run_id);
                Ok(resp)
            }
            Gate::Allow(inject) => forward_websocket(req, host, port, inject).await,
        };
    }

    let had_scheme = req.uri().scheme().is_some();
    let (parts, incoming) = req.into_parts();
    let body_bytes = incoming
        .collect()
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_default();
    let mcp = lift_mcp(&headers, &body_bytes);
    let gr = GovernedRequest {
        worker: Some(subject.clone()),
        protocol: protocol.into(),
        method: parts.method.as_str().to_string(),
        host: host.clone(),
        port,
        path: parts.uri.path().to_string(),
        query: parts.uri.query().unwrap_or("").to_string(),
        headers,
        body_size: body_bytes.len() as u64,
        mcp,
    };

    let inject = match gate(&gr, &subject).await {
        Gate::Deny(resp) => {
            note_refusal(&run_id);
            return Ok(resp);
        }
        Gate::Allow(inject) => inject,
    };

    // Forward with the buffered body, swapping the sentinel for the real
    // credential (injected headers overwrite the box's).
    let path = &gr.path;
    let query = &gr.query;
    let target: hyper::Uri = if had_scheme {
        parts.uri.clone()
    } else if query.is_empty() {
        format!("https://{host}{path}").parse()?
    } else {
        format!("https://{host}{path}?{query}").parse()?
    };
    let inject_keys: std::collections::HashSet<&str> =
        inject.iter().map(|(k, _)| k.as_str()).collect();
    let mut builder = Request::builder().method(parts.method.clone()).uri(target);
    for (k, v) in parts.headers.iter() {
        if k == hyper::header::PROXY_AUTHORIZATION || inject_keys.contains(k.as_str()) {
            continue; // drop hop-by-hop; drop headers we're about to inject
        }
        builder = builder.header(k, v);
    }
    for (k, v) in &inject {
        builder = builder.header(k, v);
    }
    let out = builder.body(
        Full::new(body_bytes)
            .map_err(|never| match never {})
            .boxed(),
    )?;
    match client.request(out).await {
        Ok(resp) => {
            let (parts, body) = resp.into_parts();
            Ok(Response::from_parts(
                parts,
                body.map_err(|e| Box::new(e) as BErr).boxed(),
            ))
        }
        Err(err) => {
            let mut resp = Response::new(full(&format!("{{\"error\":\"upstream: {err}\"}}")));
            *resp.status_mut() = StatusCode::BAD_GATEWAY;
            Ok(resp)
        }
    }
}

/// Proxy a WebSocket upgrade: send the (injected) handshake to the real host,
/// and on 101 tunnel the frames bidirectionally. TLS is already terminated, so
/// injection applies to the handshake just like an HTTP request.
async fn forward_websocket(
    mut req: Request<Incoming>,
    host: String,
    port: u16,
    inject: Vec<(String, String)>,
) -> Result<Response<Body>, BErr> {
    let box_upgrade = hyper::upgrade::on(&mut req); // resolves after we return 101
    let (parts, _body) = req.into_parts();

    // Open our own verified TLS connection to the real host and speak HTTP/1.
    let tcp = tokio::net::TcpStream::connect((host.as_str(), port)).await?;
    let server_name = rustls::pki_types::ServerName::try_from(host.clone())?;
    let tls = upstream_connector().connect(server_name, tcp).await?;
    let (mut sender, conn) =
        hyper::client::conn::http1::handshake::<_, Body>(TokioIo::new(tls)).await?;
    tokio::spawn(async move {
        let _ = conn.with_upgrades().await;
    });

    // Replay the handshake (origin-form), injecting the credential.
    let pq = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/")
        .to_string();
    let inject_keys: std::collections::HashSet<&str> =
        inject.iter().map(|(k, _)| k.as_str()).collect();
    let mut builder = Request::builder().method(parts.method.clone()).uri(pq);
    for (k, v) in parts.headers.iter() {
        if k == hyper::header::PROXY_AUTHORIZATION || inject_keys.contains(k.as_str()) {
            continue;
        }
        builder = builder.header(k, v);
    }
    for (k, v) in &inject {
        builder = builder.header(k, v);
    }
    let out = builder.body(
        Empty::<Bytes>::new()
            .map_err(|never| match never {})
            .boxed(),
    )?;

    let resp = sender.send_request(out).await?;
    if resp.status() != StatusCode::SWITCHING_PROTOCOLS {
        // Upstream declined the upgrade — pass its response back as-is.
        let (rp, body) = resp.into_parts();
        return Ok(Response::from_parts(
            rp,
            body.map_err(|e| Box::new(e) as BErr).boxed(),
        ));
    }

    // Both sides upgraded: tunnel the raw frames.
    let resp_headers = resp.headers().clone();
    let upstream_upgrade = hyper::upgrade::on(resp);
    tokio::spawn(async move {
        if let (Ok(a), Ok(b)) = (box_upgrade.await, upstream_upgrade.await) {
            let mut a = TokioIo::new(a);
            let mut b = TokioIo::new(b);
            let _ = tokio::io::copy_bidirectional(&mut a, &mut b).await;
        }
    });

    let mut response = Response::new(empty());
    *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    *response.headers_mut() = resp_headers;
    Ok(response)
}

/// A TLS client that verifies real hosts with the system roots (for the WS
/// upstream connection, where we need the raw upgraded stream).
fn upstream_connector() -> tokio_rustls::TlsConnector {
    static CONNECTOR: std::sync::OnceLock<tokio_rustls::TlsConnector> = std::sync::OnceLock::new();
    CONNECTOR
        .get_or_init(|| {
            let mut roots = rustls::RootCertStore::empty();
            for cert in rustls_native_certs::load_native_certs().certs {
                let _ = roots.add(cert);
            }
            let config = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            tokio_rustls::TlsConnector::from(Arc::new(config))
        })
        .clone()
}

// ── Regression guard: F1 — a box cannot impersonate another worker ───────────
//
// The former exploit, now asserted CLOSED, against the REAL gateway code
// (`resolve_identity` + `judge`/`scope`), no Docker/daemon needed.
//
// Run:  cargo test poc_f1 -- --nocapture
//
// The attack was: the box fully controls the CONNECT `Proxy-Authorization`
// header (it has a shell + curl), and `resolve_identity` fed the username
// straight into a filesystem path. Because run dirs are bind-mounted writable,
// the box could write a forged `{ "subject": … }` file and point the token at
// its absolute path — an absolute path makes `identity_dir().join(...)` discard
// `identity_dir` entirely — impersonating any worker. The fix validates the
// token is a minted UUID before any filesystem access and fails closed to an
// un-grantable subject otherwise.
#[cfg(test)]
mod poc_f1_identity_spoof {
    use super::{resolve_identity, UNRESOLVED_SUBJECT};
    use crate::gateway::judge::judge;
    use crate::gateway::schema::{GovernedRequest, Policy, Verdict};
    use base64::Engine as _;
    use std::collections::HashMap;

    /// Build a `Proxy-Authorization: Basic …` value the way curl would from
    /// `--proxy-user '<username>:x'`. `username` is the attacker-chosen "token".
    fn proxy_auth(username: &str) -> hyper::header::HeaderValue {
        let b64 = base64::engine::general_purpose::STANDARD.encode(format!("{username}:x"));
        hyper::header::HeaderValue::from_str(&format!("Basic {b64}")).unwrap()
    }

    /// A GET only the *victim* worker (org/finance-bot) is allowed to make; the
    /// gateway would also inject the victim's GitHub PAT in transit on it.
    fn victim_request(subject: &str) -> GovernedRequest {
        GovernedRequest {
            worker: Some(subject.to_string()),
            protocol: "https".into(),
            method: "GET".into(),
            host: "api.github.com".into(),
            port: 443,
            path: "/user/repos".into(),
            query: String::new(),
            headers: HashMap::new(),
            body_size: 0,
            mcp: None,
        }
    }

    /// Admin policy: only org/finance-bot may reach api.github.com, injecting
    /// its credential. Any other subject falls through to default-deny.
    fn policy() -> Policy {
        serde_json::from_str(
            r#"{"rules":[{
                "name":"finance-bot-github",
                "scope":"org/finance-bot",
                "match":{"host":"api.github.com","port":443,"method":"GET"},
                "verdict":"allow",
                "inject":{"credential":"finance_bot_github_pat"}
            }]}"#,
        )
        .unwrap()
    }

    #[test]
    fn poc_f1_box_cannot_assume_another_workers_identity() {
        let pol = policy();

        // A scratch dir standing in for the box's writable workspace mount. In a
        // real deployment this is $ROSTER_ROOT/state/runs/<run_id>/workspace —
        // writable, and the box knows its absolute path (via pwd).
        let workspace = std::env::temp_dir().join(format!(
            "roster-f1-{}/runs/run-abc/workspace",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&workspace).unwrap();

        // The former exploit: forge an identity file and present its absolute
        // path as the proxy token. The path is not a UUID, so resolution now
        // rejects it BEFORE reading the file — no impersonation.
        let forged = workspace.join("evil.json");
        std::fs::write(
            &forged,
            r#"{"subject":"org/finance-bot","run_id":"forged"}"#,
        )
        .unwrap();
        let (spoofed, _) = resolve_identity(Some(&proxy_auth(
            forged.with_extension("").to_str().unwrap(),
        )));
        assert_eq!(
            spoofed, UNRESOLVED_SUBJECT,
            "F1: absolute-path token must not resolve to a worker"
        );
        assert_ne!(spoofed, "org/finance-bot", "F1: no impersonation");
        assert_eq!(
            judge(&victim_request(&spoofed), &pol).0,
            Verdict::Deny,
            "F1: no grant"
        );

        // A `..` traversal token is likewise rejected (not a UUID).
        let (trav, _) = resolve_identity(Some(&proxy_auth("../../../../etc/passwd")));
        assert_eq!(trav, UNRESOLVED_SUBJECT, "F1: traversal token rejected");

        // Junk fails CLOSED to an un-grantable subject — no longer the fleet root.
        let (junk, _) = resolve_identity(Some(&proxy_auth("not-a-real-token")));
        assert_eq!(
            junk, UNRESOLVED_SUBJECT,
            "F1: unknown token no longer becomes org"
        );
        assert_eq!(judge(&victim_request(&junk), &pol).0, Verdict::Deny);

        // A well-formed but UNREGISTERED UUID (no file on disk) also fails closed.
        let (ghost, _) = resolve_identity(Some(&proxy_auth(&uuid::Uuid::new_v4().to_string())));
        assert_eq!(
            ghost, UNRESOLVED_SUBJECT,
            "F1: valid-shape but unknown token denied"
        );

        // A host-side caller (the roster binary itself) sends NO proxy auth and
        // still resolves to the org subject — the legitimate path is preserved.
        assert_eq!(
            resolve_identity(None).0,
            "org",
            "host-side default preserved"
        );

        println!("\n=== F1 regression guard: identity spoofing is blocked ===");
        println!("absolute-path token -> {spoofed}   (was: org/finance-bot)");
        println!("traversal token     -> {trav}");
        println!("junk token          -> {junk}   (was: org)");
        println!("unregistered UUID   -> {ghost}");
        println!("no proxy auth       -> org (host-side, preserved)");
        println!("=> every box-controlled token now default-denies; #11 holds.\n");

        let _ = std::fs::remove_dir_all(workspace.parent().unwrap().parent().unwrap());
    }
}
