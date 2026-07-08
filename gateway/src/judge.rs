//! The judge: pure `(request, policy) -> verdict`. First matching rule wins;
//! no match denies. Ports `src/judge.ts` exactly (structured matcher; CEL comes
//! with the metering increment, D18). See docs/rust-port.md (P2).

use crate::schema::{GovernedRequest, Match, Policy, Verdict};

pub fn judge(req: &GovernedRequest, policy: &Policy) -> (Verdict, Option<String>) {
    let subject = req.worker.as_deref().unwrap_or("org");
    for rule in &policy.rules {
        if crate::scope::applies(&rule.scope, subject) && matches(&rule.r#match, req) {
            return (rule.verdict, Some(rule.name.clone()));
        }
    }
    (Verdict::Deny, None)
}

fn matches(m: &Match, req: &GovernedRequest) -> bool {
    if let Some(p) = &m.protocol {
        if !p.values().iter().any(|v| v.as_str() == req.protocol) {
            return false;
        }
    }
    if let Some(h) = &m.host {
        if !h.values().iter().any(|pat| host_matches(pat, &req.host)) {
            return false;
        }
    }
    if let Some(port) = &m.port {
        if !port.values().iter().any(|v| **v == req.port) {
            return false;
        }
    }
    if let Some(meth) = &m.method {
        if !meth.values().iter().any(|v| v.eq_ignore_ascii_case(&req.method)) {
            return false;
        }
    }
    if let Some(pp) = &m.path_prefix {
        if !req.path.starts_with(pp) {
            return false;
        }
    }
    if let Some(mb) = m.max_body_size {
        if req.body_size > mb {
            return false;
        }
    }
    if let Some(hc) = &m.header_contains {
        for (name, sub) in hc {
            match req.headers.get(&name.to_lowercase()) {
                None => return false,
                Some(val) => {
                    if !sub.is_empty() && !val.contains(sub) {
                        return false;
                    }
                }
            }
        }
    }
    if let Some(mcpm) = &m.mcp {
        match &req.mcp {
            None => return false,
            Some(mcp) => {
                if let Some(meth) = &mcpm.method {
                    if !meth.values().iter().any(|v| v.as_str() == mcp.method) {
                        return false;
                    }
                }
                if let Some(tool) = &mcpm.tool {
                    match &mcp.tool {
                        None => return false,
                        Some(t) => {
                            if !tool.values().iter().any(|pat| glob_matches(pat, t)) {
                                return false;
                            }
                        }
                    }
                }
            }
        }
    }
    true
}

/// exact | `*` (any) | `*.suffix` (suffix itself or any label under it).
pub fn host_matches(pattern: &str, host: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix("*.") {
        return host == suffix || host.ends_with(&format!(".{suffix}"));
    }
    pattern == host
}

/// Classic `*`-glob (no `?`), anchored — matches `src/judge.ts` globMatches.
pub fn glob_matches(pattern: &str, s: &str) -> bool {
    fn go(p: &[u8], s: &[u8]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some(b'*') => go(&p[1..], s) || (!s.is_empty() && go(p, &s[1..])),
            Some(&c) => !s.is_empty() && s[0] == c && go(&p[1..], &s[1..]),
        }
    }
    go(pattern.as_bytes(), s.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Mcp;
    use std::collections::HashMap;

    fn req() -> GovernedRequest {
        let mut headers = HashMap::new();
        headers.insert("authorization".into(), "Bearer x".into());
        headers.insert("content-type".into(), "application/json".into());
        GovernedRequest {
            worker: None,
            protocol: "https".into(),
            method: "POST".into(),
            host: "chatgpt.com".into(),
            port: 443,
            path: "/backend-api/codex/responses".into(),
            query: String::new(),
            headers,
            body_size: 100,
            mcp: None,
        }
    }

    fn policy(json: &str) -> Policy {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn default_deny_on_empty_policy() {
        let (v, r) = judge(&req(), &Policy::empty());
        assert_eq!(v, Verdict::Deny);
        assert!(r.is_none());
    }

    #[test]
    fn first_match_wins() {
        let p = policy(r#"{"rules":[{"name":"d","match":{},"verdict":"deny"},{"name":"a","match":{},"verdict":"allow"}]}"#);
        let (v, r) = judge(&req(), &p);
        assert_eq!(v, Verdict::Deny);
        assert_eq!(r.as_deref(), Some("d"));
    }

    #[test]
    fn host_port_and_method() {
        let p = policy(r#"{"rules":[{"name":"model","match":{"host":["chatgpt.com","api.anthropic.com"],"port":443},"verdict":"allow"}]}"#);
        assert_eq!(judge(&req(), &p).0, Verdict::Allow);
        let mut evil = req();
        evil.host = "evil.com".into();
        assert_eq!(judge(&evil, &p).0, Verdict::Deny);

        let pm = policy(r#"{"rules":[{"name":"posts","match":{"host":"chatgpt.com","method":"post"},"verdict":"allow"}]}"#);
        let mut get = req();
        get.method = "GET".into();
        assert_eq!(judge(&get, &pm).0, Verdict::Deny);
        assert_eq!(judge(&req(), &pm).0, Verdict::Allow);
    }

    #[test]
    fn mcp_tool_globs() {
        let p = policy(r#"{"rules":[{"name":"ro","match":{"mcp":{"method":"tools/call","tool":["get_*","list_*"]}},"verdict":"allow"}]}"#);
        let mut r = req();
        r.mcp = Some(Mcp { method: "tools/call".into(), tool: Some("get_issue".into()) });
        assert_eq!(judge(&r, &p).0, Verdict::Allow);
        r.mcp = Some(Mcp { method: "tools/call".into(), tool: Some("create_pr".into()) });
        assert_eq!(judge(&r, &p).0, Verdict::Deny);
        r.mcp = None;
        assert_eq!(judge(&r, &p).0, Verdict::Deny);
    }

    #[test]
    fn rule_scope_is_ancestor_filtered() {
        // A rule scoped to org/w1 must not govern a request from org/w2.
        let p = policy(r#"{"rules":[{"name":"w1-only","match":{"host":"chatgpt.com"},"verdict":"allow","scope":"org/w1"}]}"#);
        let mut r = req();
        r.worker = Some("org/w1".into());
        assert_eq!(judge(&r, &p).0, Verdict::Allow);
        r.worker = Some("org/w2".into());
        assert_eq!(judge(&r, &p).0, Verdict::Deny); // out of scope → no rule → default deny
        // An org-scoped rule governs any worker.
        let org = policy(r#"{"rules":[{"name":"all","match":{"host":"chatgpt.com"},"verdict":"allow","scope":"org"}]}"#);
        assert_eq!(judge(&r, &org).0, Verdict::Allow);
    }

    #[test]
    fn host_and_glob_helpers() {
        assert!(host_matches("*", "anything.com"));
        assert!(host_matches("*.githubcopilot.com", "api.githubcopilot.com"));
        assert!(host_matches("*.githubcopilot.com", "githubcopilot.com"));
        assert!(!host_matches("*.githubcopilot.com", "githubcopilot.com.evil.com"));
        assert!(!host_matches("chatgpt.com", "evil.chatgpt.com"));
        assert!(glob_matches("get_*", "get_issue"));
        assert!(!glob_matches("get_*", "set_issue"));
        assert!(!glob_matches("get_*", "xget_issue"));
    }
}
