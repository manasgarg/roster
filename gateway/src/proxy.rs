//! The proxy core: accept a connection, answer /healthz, and for CONNECT either
//! raw-tunnel (the `tunnel` verdict, for cert-pinning clients) or terminate TLS
//! and judge each decrypted request before forwarding. Ports the server +
//! judge + forward loop in `src/gateway.ts`. Injection/refresh land in P3.
//! See docs/rust-port.md (P2).

use crate::ca::Ca;
use crate::judge::judge;
use crate::schema::{GovernedRequest, Mcp, Policy, Verdict};
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
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use time::format_description::well_known::Rfc3339;
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

fn root() -> PathBuf {
    std::env::var("ROSTER_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Read the policy fresh each decision so owner edits are live. Fail closed: an
/// unparseable policy denies everything (empty rule list).
fn load_policy() -> Policy {
    let path = root().join("policies").join("gateway.json");
    match std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str::<Policy>(&s).ok()) {
        Some(p) => p,
        None => {
            eprintln!("gateway: policy unreadable at {} — denying all", path.display());
            Policy::empty()
        }
    }
}

// ── decision log ────────────────────────────────────────────────────────────

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_default()
}

fn next_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{nanos:x}-{n:x}")
}

fn record(gr: &GovernedRequest, verdict: Verdict, rule: Option<&str>, injected: Option<&[String]>) {
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
    });
    if let Some(inj) = injected {
        dec["injected"] = json!(inj);
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

        // Tunnel escape hatch: judge host+port only; if the rule says tunnel,
        // raw-pipe without terminating (host-only visibility).
        let pre = GovernedRequest {
            worker: None,
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
            record(&pre, Verdict::Tunnel, rule.as_deref(), None);
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
            let svc = service_fn(move |r| handle(r, "https", host.clone(), client.clone()));
            let _ = server_http1::Builder::new().serve_connection(io, svc).await;
        });
        Ok(Response::new(empty()))
    } else if req.uri().path() == "/healthz" {
        let mut resp = Response::new(full("{\"ok\":true}"));
        resp.headers_mut().insert(hyper::header::CONTENT_TYPE, "application/json".parse().unwrap());
        Ok(resp)
    } else if req.uri().scheme_str() == Some("http") {
        let host = req.uri().host().unwrap_or("").to_string();
        handle(req, "http", host, client).await
    } else {
        let mut resp = Response::new(full("{\"error\":\"not a proxy request\"}"));
        *resp.status_mut() = StatusCode::BAD_REQUEST;
        Ok(resp)
    }
}

/// A decrypted (or plain-http) request: buffer body, judge, record, then deny
/// or forward. Response streams back.
async fn handle(req: Request<Incoming>, protocol: &str, host: String, client: UpstreamClient) -> Result<Response<Body>, BErr> {
    let (parts, incoming) = req.into_parts();
    let path = parts.uri.path().to_string();
    let query = parts.uri.query().unwrap_or("").to_string();
    let method = parts.method.as_str().to_string();
    let headers = lower_headers(&parts.headers);
    let port: u16 = if protocol == "https" { 443 } else { 80 };
    let had_scheme = parts.uri.scheme().is_some();

    let body_bytes = incoming.collect().await.map(|c| c.to_bytes()).unwrap_or_default();
    let mcp = lift_mcp(&headers, &body_bytes);
    let gr = GovernedRequest {
        worker: None,
        protocol: protocol.into(),
        method: method.clone(),
        host: host.clone(),
        port,
        path: path.clone(),
        query: query.clone(),
        headers,
        body_size: body_bytes.len() as u64,
        mcp,
    };

    let (verdict, rule) = judge(&gr, &load_policy());
    record(&gr, verdict, rule.as_deref(), None);
    if verdict != Verdict::Allow {
        return Ok(deny_response(verdict, rule.as_deref()));
    }

    // Forward with the buffered body.
    let target: hyper::Uri = if had_scheme {
        parts.uri.clone()
    } else if query.is_empty() {
        format!("https://{host}{path}").parse()?
    } else {
        format!("https://{host}{path}?{query}").parse()?
    };
    let mut builder = Request::builder().method(parts.method.clone()).uri(target);
    for (k, v) in parts.headers.iter() {
        if k != hyper::header::PROXY_AUTHORIZATION {
            builder = builder.header(k, v);
        }
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
