//! The proxy core: accept a connection, answer /healthz, and for CONNECT either
//! raw-tunnel (the `tunnel` verdict, for cert-pinning clients) or terminate TLS
//! and judge each decrypted request before forwarding. Ports the server +
//! judge + forward loop in `src/gateway.ts`. Injection/refresh land in P3.
//! See docs/rust-port.md (P2).

use crate::ca::Ca;
use crate::judge::judge;
use crate::schema::{GovernedRequest, Mcp, Policy, Verdict};
use crate::util::{now_rfc3339, root};
use crate::vault;
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
use std::fs::OpenOptions;
use std::io::Write;
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

/// Read the policy fresh each decision so owner edits are live. Fail closed: an
/// unparseable policy denies everything (empty rule list).
fn load_policy() -> Policy {
    let path = root().join("runs").join("compiled").join("policy.json");
    match std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str::<Policy>(&s).ok()) {
        Some(p) => p,
        None => {
            eprintln!("gateway: no compiled policy at {} — denying all (run: roster deploy)", path.display());
            Policy::empty()
        }
    }
}

// ── decision log ────────────────────────────────────────────────────────────

fn next_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
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
            let val = if SENSITIVE.contains(&k.as_str()) { "<redacted>".to_string() } else { v.clone() };
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

    let path = root().join("runs").join("decisions.jsonl");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{dec}");
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
    let ct = headers.get("content-type").map(|s| s.as_str()).unwrap_or("");
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
        msg.get("params").and_then(|p| p.get("name")).and_then(|n| n.as_str()).map(|s| s.to_string())
    } else {
        None
    };
    Some(Mcp { method, tool })
}

// ── body helpers ────────────────────────────────────────────────────────────

fn full(s: &str) -> Body {
    Full::new(Bytes::from(s.to_string())).map_err(|never| match never {}).boxed()
}

fn empty() -> Body {
    Empty::<Bytes>::new().map_err(|never| match never {}).boxed()
}

// ── identity ────────────────────────────────────────────────────────────────

/// Resolve the call's subject and run from the CONNECT's Proxy-Authorization. The
/// trusted runner sets `HTTP(S)_PROXY=http://<token>@…` and registers
/// `~/.roster/identity/<token>.json = {subject}` (off the box mount), so the box
/// can present only its own random token — it can't claim another worker's
/// identity. Unknown/absent ⇒ "org" (host-side tools with no creds).
fn resolve_identity(proxy_auth: Option<&hyper::header::HeaderValue>) -> (String, String) {
    let default = || ("org".to_string(), String::new());
    let Some(b64) = proxy_auth
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Basic "))
    else {
        return default();
    };
    use base64::Engine;
    let token = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .ok()
        .and_then(|d| String::from_utf8(d).ok())
        .map(|creds| creds.split(':').next().unwrap_or("").to_string())
        .unwrap_or_default();
    if token.is_empty() {
        return default();
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let path = std::path::Path::new(&home).join(".roster").join("identity").join(format!("{token}.json"));
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| {
            let subject = v.get("subject")?.as_str()?.to_string();
            let run_id = v.get("run_id").and_then(Value::as_str).unwrap_or("").to_string();
            Some((subject, run_id))
        })
        .unwrap_or_else(default)
}

fn deny_response(verdict: Verdict, rule: Option<&str>) -> Response<Body> {
    let rule_json = rule.map(|r| format!("\"{r}\"")).unwrap_or_else(|| "null".into());
    let mut resp = Response::new(full(&format!(
        "{{\"error\":\"denied by gateway ({})\",\"rule\":{}}}",
        verdict.as_str(),
        rule_json
    )));
    *resp.status_mut() = StatusCode::FORBIDDEN;
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
    if let Err(e) = server_http1::Builder::new().serve_connection(io, svc).with_upgrades().await {
        let _ = e;
    }
}

async fn outer(req: Request<Incoming>, tls: TlsAcceptor, client: UpstreamClient) -> Result<Response<Body>, BErr> {
    if req.method() == Method::CONNECT {
        let authority = req.uri().authority().map(|a| a.to_string()).unwrap_or_default();
        let host = authority.split(':').next().unwrap_or("").to_string();
        let port: u16 = authority.split(':').nth(1).and_then(|p| p.parse().ok()).unwrap_or(443);
        let (subject, run_id) = resolve_identity(req.headers().get(hyper::header::PROXY_AUTHORIZATION));

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
            record(&pre, Verdict::Tunnel, rule.as_deref(), None, &HashMap::new(), None);
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
            let svc = service_fn(move |r| handle(r, "https", host.clone(), subject.clone(), run_id.clone(), client.clone()));
            let _ = server_http1::Builder::new().serve_connection(io, svc).with_upgrades().await;
        });
        Ok(Response::new(empty()))
    } else if req.uri().path() == "/healthz" {
        let mut resp = Response::new(full("{\"ok\":true}"));
        resp.headers_mut().insert(hyper::header::CONTENT_TYPE, "application/json".parse().unwrap());
        Ok(resp)
    } else if req.uri().scheme_str() == Some("http") {
        let host = req.uri().host().unwrap_or("").to_string();
        let (subject, run_id) = resolve_identity(req.headers().get(hyper::header::PROXY_AUTHORIZATION));
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
                        record(gr, Verdict::Deny, rule.as_deref(), None, &HashMap::new(), None);
                        return Gate::Deny(deny_response(Verdict::Deny, rule.as_deref()));
                    }
                    Ok(Some(cred)) => {
                        inject = vault::render_injection(&cred, &inj.credential);
                        injected_names = Some(inject.iter().map(|(k, _)| k.clone()).collect());
                    }
                }
            }
        }
    }

    // Meter + enforce the budget against the call's subject (ancestor rollup).
    let budget = crate::budget::load_budget();
    let now = crate::util::now_ms();
    let spend = if verdict == Verdict::Allow {
        crate::budget::compute_spend(gr, verdict.as_str(), rule.as_deref(), &json!({}), &budget)
    } else {
        HashMap::new()
    };
    if verdict == Verdict::Allow {
        if let Some(reason) = crate::ledger::check(subject, &spend, &budget.limits, now) {
            record(gr, Verdict::Deny, rule.as_deref(), None, &HashMap::new(), Some(&reason));
            let mut resp = Response::new(full(&format!("{{\"error\":\"budget exceeded\",\"detail\":\"{reason}\"}}")));
            *resp.status_mut() = StatusCode::PAYMENT_REQUIRED;
            return Gate::Deny(resp);
        }
    }

    record(gr, verdict, rule.as_deref(), injected_names.as_deref(), &spend, None);
    if verdict != Verdict::Allow {
        return Gate::Deny(deny_response(verdict, rule.as_deref()));
    }
    crate::ledger::debit(subject, &spend, &budget.limits, now);
    Gate::Allow(inject)
}

/// A decrypted (or plain-http) request: judge, then forward. WebSocket upgrades
/// are tunneled (see forward_websocket); everything else is a buffered forward
/// with the response streamed back.
async fn handle(req: Request<Incoming>, protocol: &str, host: String, subject: String, run_id: String, client: UpstreamClient) -> Result<Response<Body>, BErr> {
    // The action host is served internally: parse the envelope and let the
    // action layer attribute, authorize, and execute-or-gate it. Never forwarded.
    if host == crate::action::ACTION_HOST {
        let (parts, incoming) = req.into_parts();
        let method = parts.method.as_str().to_string();
        let path = parts.uri.path().to_string();
        let body = incoming.collect().await.map(|c| c.to_bytes()).unwrap_or_default();
        return Ok(crate::action::handle_action(&subject, &run_id, &method, &path, &body).await);
    }

    let headers = lower_headers(req.headers());
    let is_ws = headers.get("upgrade").map(|u| u.eq_ignore_ascii_case("websocket")).unwrap_or(false);
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
            Gate::Deny(resp) => Ok(resp),
            Gate::Allow(inject) => forward_websocket(req, host, port, inject).await,
        };
    }

    let had_scheme = req.uri().scheme().is_some();
    let (parts, incoming) = req.into_parts();
    let body_bytes = incoming.collect().await.map(|c| c.to_bytes()).unwrap_or_default();
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
        Gate::Deny(resp) => return Ok(resp),
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
    let inject_keys: std::collections::HashSet<&str> = inject.iter().map(|(k, _)| k.as_str()).collect();
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
    let out = builder.body(Full::new(body_bytes).map_err(|never| match never {}).boxed())?;
    match client.request(out).await {
        Ok(resp) => {
            let (parts, body) = resp.into_parts();
            Ok(Response::from_parts(parts, body.map_err(|e| Box::new(e) as BErr).boxed()))
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
async fn forward_websocket(mut req: Request<Incoming>, host: String, port: u16, inject: Vec<(String, String)>) -> Result<Response<Body>, BErr> {
    let box_upgrade = hyper::upgrade::on(&mut req); // resolves after we return 101
    let (parts, _body) = req.into_parts();

    // Open our own verified TLS connection to the real host and speak HTTP/1.
    let tcp = tokio::net::TcpStream::connect((host.as_str(), port)).await?;
    let server_name = rustls::pki_types::ServerName::try_from(host.clone())?;
    let tls = upstream_connector().connect(server_name, tcp).await?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake::<_, Body>(TokioIo::new(tls)).await?;
    tokio::spawn(async move {
        let _ = conn.with_upgrades().await;
    });

    // Replay the handshake (origin-form), injecting the credential.
    let pq = parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/").to_string();
    let inject_keys: std::collections::HashSet<&str> = inject.iter().map(|(k, _)| k.as_str()).collect();
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
    let out = builder.body(Empty::<Bytes>::new().map_err(|never| match never {}).boxed())?;

    let resp = sender.send_request(out).await?;
    if resp.status() != StatusCode::SWITCHING_PROTOCOLS {
        // Upstream declined the upgrade — pass its response back as-is.
        let (rp, body) = resp.into_parts();
        return Ok(Response::from_parts(rp, body.map_err(|e| Box::new(e) as BErr).boxed()));
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
            let config = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
            tokio_rustls::TlsConnector::from(Arc::new(config))
        })
        .clone()
}
