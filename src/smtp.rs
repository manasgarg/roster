//! A minimal SMTP-over-TLS sender for the email executor. Direct trusted-side
//! egress (the gateway is an HTTP proxy and can't carry SMTP), like the git-push
//! executor — the box never speaks SMTP or holds the credential; the send only
//! happens after the action is governed. Implicit TLS (port 465) + AUTH LOGIN,
//! which is what Mailgun's SMTP endpoint speaks. No new dependencies: reuses the
//! rustls/tokio stack the gateway already links.

use base64::Engine;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use time::format_description::well_known::Rfc2822;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

/// Whole-conversation deadline. Kept under the box's 30s action-tool timeout so
/// a stuck send surfaces as a clear SMTP error, not an opaque tool timeout.
const SEND_TIMEOUT_SECS: u64 = 20;

pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub pass: String,
    /// The pinned sender ("Name <addr@domain>" or "addr@domain"). The worker
    /// never chooses this — governance owns the identity mail goes out under.
    pub from: String,
}

/// Send one message, bounded by a deadline. Returns a short status on success,
/// or a diagnostic string (never hangs).
pub async fn send(cfg: &SmtpConfig, to: &[String], subject: &str, body: &str) -> Result<String, String> {
    match tokio::time::timeout(Duration::from_secs(SEND_TIMEOUT_SECS), send_inner(cfg, to, subject, body)).await {
        Ok(r) => r,
        Err(_) => Err(format!(
            "timed out after {SEND_TIMEOUT_SECS}s talking to {}:{} — is the port reachable? (Mailgun also offers 587)",
            cfg.host, cfg.port
        )),
    }
}

async fn send_inner(cfg: &SmtpConfig, to: &[String], subject: &str, body: &str) -> Result<String, String> {
    let tcp = TcpStream::connect((cfg.host.as_str(), cfg.port)).await.map_err(|e| format!("connect {}:{}: {e}", cfg.host, cfg.port))?;
    let name = rustls::pki_types::ServerName::try_from(cfg.host.clone()).map_err(|e| format!("bad host: {e}"))?;
    let tls = connector().connect(name, tcp).await.map_err(|e| format!("TLS: {e}"))?;
    let (rd, mut wr) = tokio::io::split(tls);
    let mut r = BufReader::new(rd);

    expect(&mut r, 220).await?; // greeting
    say(&mut wr, &mut r, "EHLO roster", 250).await?;
    say(&mut wr, &mut r, "AUTH LOGIN", 334).await?;
    say(&mut wr, &mut r, &b64(&cfg.user), 334).await?;
    say(&mut wr, &mut r, &b64(&cfg.pass), 235).await?;
    say(&mut wr, &mut r, &format!("MAIL FROM:<{}>", addr_of(&cfg.from)), 250).await?;
    for rcpt in to {
        say(&mut wr, &mut r, &format!("RCPT TO:<{}>", addr_of(rcpt)), 250).await?;
    }
    say(&mut wr, &mut r, "DATA", 354).await?;

    let message = build_message(cfg, to, subject, body);
    wr.write_all(message.as_bytes()).await.map_err(|e| e.to_string())?;
    wr.write_all(b".\r\n").await.map_err(|e| e.to_string())?;
    expect(&mut r, 250).await?; // accepted for delivery
    let _ = say(&mut wr, &mut r, "QUIT", 221).await;
    Ok("queued for delivery".to_string())
}

fn connector() -> TlsConnector {
    static C: OnceLock<TlsConnector> = OnceLock::new();
    C.get_or_init(|| {
        // The email executor can run from the CLI (`gates approve`), where the
        // gateway's provider install hasn't run — install idempotently.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_native_certs::load_native_certs().certs {
            let _ = roots.add(cert);
        }
        // Extra trust anchor for an SMTP relay behind a private CA (set
        // ROSTER_SMTP_CA to a PEM file). Public relays like Mailgun don't need it.
        if let Ok(path) = std::env::var("ROSTER_SMTP_CA") {
            for cert in load_pem_certs(&path) {
                let _ = roots.add(cert);
            }
        }
        let config = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
        TlsConnector::from(Arc::new(config))
    })
    .clone()
}

fn load_pem_certs(path: &str) -> Vec<rustls::pki_types::CertificateDer<'static>> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut certs = Vec::new();
    let mut acc = String::new();
    let mut inside = false;
    for line in text.lines() {
        if line.contains("BEGIN CERTIFICATE") {
            inside = true;
            acc.clear();
        } else if line.contains("END CERTIFICATE") {
            inside = false;
            if let Ok(der) = base64::engine::general_purpose::STANDARD.decode(acc.trim()) {
                certs.push(rustls::pki_types::CertificateDer::from(der));
            }
        } else if inside {
            acc.push_str(line.trim());
        }
    }
    certs
}

// ── SMTP conversation helpers ────────────────────────────────────────────────

/// Write a command line and require a specific reply code.
async fn say<W, R>(wr: &mut W, r: &mut BufReader<R>, line: &str, code: u16) -> Result<(), String>
where
    W: AsyncWriteExt + Unpin,
    R: tokio::io::AsyncRead + Unpin,
{
    wr.write_all(line.as_bytes()).await.map_err(|e| e.to_string())?;
    wr.write_all(b"\r\n").await.map_err(|e| e.to_string())?;
    expect(r, code).await
}

/// Read one (possibly multi-line) reply and check its code. Lines look like
/// `250-...` (continuation) or `250 ...` (final); the code repeats on each.
async fn expect<R>(r: &mut BufReader<R>, code: u16) -> Result<(), String>
where
    R: tokio::io::AsyncRead + Unpin,
{
    loop {
        let mut line = String::new();
        let n = r.read_line(&mut line).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("SMTP connection closed unexpectedly".into());
        }
        let b = line.as_bytes();
        if b.len() >= 4 && b[3] == b' ' {
            let got: u16 = line[..3].parse().unwrap_or(0);
            if got != code {
                return Err(format!("SMTP expected {code}, got: {}", line.trim()));
            }
            return Ok(());
        }
        // otherwise a continuation line — keep reading
    }
}

fn b64(s: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
}

/// The bare address for the envelope: the part inside <...>, else the string.
fn addr_of(s: &str) -> String {
    match (s.find('<'), s.find('>')) {
        (Some(a), Some(b)) if b > a + 1 => s[a + 1..b].to_string(),
        _ => s.trim().to_string(),
    }
}

fn domain_of(addr: &str) -> String {
    addr.rsplit('@').next().unwrap_or("roster.local").to_string()
}

fn build_message(cfg: &SmtpConfig, to: &[String], subject: &str, body: &str) -> String {
    let date = time::OffsetDateTime::now_utc().format(&Rfc2822).unwrap_or_default();
    let id = format!("{}@{}", uuid::Uuid::new_v4().simple(), domain_of(&addr_of(&cfg.from)));
    let mut m = String::new();
    m.push_str(&format!("From: {}\r\n", cfg.from));
    m.push_str(&format!("To: {}\r\n", to.join(", ")));
    m.push_str(&format!("Subject: {subject}\r\n"));
    m.push_str(&format!("Date: {date}\r\n"));
    m.push_str(&format!("Message-ID: <{id}>\r\n"));
    m.push_str("MIME-Version: 1.0\r\n");
    m.push_str("Content-Type: text/plain; charset=utf-8\r\n\r\n");
    // Body: normalize to CRLF and dot-stuff any line beginning with '.'.
    for raw in body.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.starts_with('.') {
            m.push('.');
        }
        m.push_str(line);
        m.push_str("\r\n");
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addr_extracts_bare_address() {
        assert_eq!(addr_of("Yuko <bot@mg.example.com>"), "bot@mg.example.com");
        assert_eq!(addr_of("bot@mg.example.com"), "bot@mg.example.com");
        assert_eq!(addr_of("  spaced@x.com "), "spaced@x.com");
    }

    #[test]
    fn message_has_crlf_headers_and_dot_stuffing() {
        let cfg = SmtpConfig { host: "h".into(), port: 465, user: "u".into(), pass: "p".into(), from: "Bot <bot@mg.example.com>".into() };
        let m = build_message(&cfg, &["a@x.com".into(), "b@y.com".into()], "Hi", "normal\n.leading dot");
        assert!(m.contains("From: Bot <bot@mg.example.com>\r\n"));
        assert!(m.contains("To: a@x.com, b@y.com\r\n"));
        assert!(m.contains("Subject: Hi\r\n"));
        assert!(m.contains("\r\n\r\n")); // header/body separator
        assert!(m.contains("\r\nnormal\r\n"));
        assert!(m.contains("\r\n..leading dot\r\n")); // dot-stuffed
    }
}
