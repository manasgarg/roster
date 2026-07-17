//! The judge: pure `(request, policy) -> verdict`. First matching rule wins;
//! no match denies. See docs/gateway.md.

use crate::gateway::schema::{GovernedRequest, Match, Policy, Verdict};

pub fn judge(req: &GovernedRequest, policy: &Policy) -> (Verdict, Option<String>) {
    let subject = req.worker.as_deref().unwrap_or("org");
    for rule in &policy.rules {
        if crate::gateway::scope::applies(&rule.scope, subject) && matches(&rule.r#match, req) {
            return (rule.verdict, Some(rule.name.clone()));
        }
    }
    (Verdict::Deny, None)
}

/// When nothing matched, the deny is often one predicate away from an allow:
/// the host is governed, the verb isn't. Report the first in-scope allow rule
/// that fails ONLY on the HTTP method — (rule name, its allowed methods) — so
/// the 403 can say how to widen the grant instead of "policy said no".
pub fn method_near_miss(req: &GovernedRequest, policy: &Policy) -> Option<(String, Vec<String>)> {
    let subject = req.worker.as_deref().unwrap_or("org");
    for rule in &policy.rules {
        if rule.verdict != Verdict::Allow || !crate::gateway::scope::applies(&rule.scope, subject) {
            continue;
        }
        if !method_allowed(&rule.r#match, req) && matches_except_method(&rule.r#match, req) {
            let methods = rule
                .r#match
                .method
                .as_ref()
                .map(|m| m.values().into_iter().cloned().collect())
                .unwrap_or_default();
            return Some((rule.name.clone(), methods));
        }
    }
    None
}

/// Collapse `.`/`..`/empty segments in a URL path so a prefix check can't be
/// fooled by dot-segments that an upstream would later resolve. Purely lexical
/// (no filesystem), matching how servers normalize request-targets.
fn normalize_path(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    format!("/{}", out.join("/"))
}

fn matches(m: &Match, req: &GovernedRequest) -> bool {
    method_allowed(m, req) && matches_except_method(m, req)
}

/// The HTTP-method predicate alone: absent = any; `*` = any.
fn method_allowed(m: &Match, req: &GovernedRequest) -> bool {
    match &m.method {
        None => true,
        Some(meth) => meth
            .values()
            .iter()
            .any(|v| v.as_str() == "*" || v.eq_ignore_ascii_case(&req.method)),
    }
}

/// Every predicate except the HTTP method — split out so method_near_miss can
/// tell "wrong verb" apart from "wrong host" without re-listing the checks.
fn matches_except_method(m: &Match, req: &GovernedRequest) -> bool {
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
    if let Some(pp) = &m.path_prefix {
        // Match the NORMALIZED path: raw starts_with lets "/allowed/../admin"
        // pass the prefix while an upstream that collapses dot-segments resolves
        // it to "/admin", escaping the confinement.
        if !normalize_path(&req.path).starts_with(pp) {
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
    use crate::gateway::schema::Mcp;
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
    fn path_prefix_resists_dot_segment_escape() {
        assert_eq!(normalize_path("/v1/readonly/../admin"), "/v1/admin");
        assert_eq!(normalize_path("/v1/readonly/../../admin"), "/admin");
        assert_eq!(normalize_path("/v1/readonly/./get"), "/v1/readonly/get");
        let p = policy(
            r#"{"rules":[{"name":"ro","match":{"host":["api.example.com"],"pathPrefix":"/v1/readonly/"},"verdict":"allow"}]}"#,
        );
        let mut good = req();
        good.host = "api.example.com".into();
        good.path = "/v1/readonly/thing".into();
        assert_eq!(judge(&good, &p).0, Verdict::Allow);
        let mut escape = req();
        escape.host = "api.example.com".into();
        escape.path = "/v1/readonly/../admin".into();
        assert_eq!(judge(&escape, &p).0, Verdict::Deny);
    }

    #[test]
    fn first_match_wins() {
        let p = policy(
            r#"{"rules":[{"name":"d","match":{},"verdict":"deny"},{"name":"a","match":{},"verdict":"allow"}]}"#,
        );
        let (v, r) = judge(&req(), &p);
        assert_eq!(v, Verdict::Deny);
        assert_eq!(r.as_deref(), Some("d"));
    }

    #[test]
    fn host_port_and_method() {
        let p = policy(
            r#"{"rules":[{"name":"model","match":{"host":["chatgpt.com","api.anthropic.com"],"port":443},"verdict":"allow"}]}"#,
        );
        assert_eq!(judge(&req(), &p).0, Verdict::Allow);
        let mut evil = req();
        evil.host = "evil.com".into();
        assert_eq!(judge(&evil, &p).0, Verdict::Deny);

        let pm = policy(
            r#"{"rules":[{"name":"posts","match":{"host":"chatgpt.com","method":"post"},"verdict":"allow"}]}"#,
        );
        let mut get = req();
        get.method = "GET".into();
        assert_eq!(judge(&get, &pm).0, Verdict::Deny);
        assert_eq!(judge(&req(), &pm).0, Verdict::Allow);
    }

    #[test]
    fn method_star_matches_any_verb() {
        let p = policy(
            r#"{"rules":[{"name":"full","match":{"host":"chatgpt.com","method":"*"},"verdict":"allow"}]}"#,
        );
        for verb in ["GET", "POST", "PATCH", "DELETE", "HEAD"] {
            let mut r = req();
            r.method = verb.into();
            assert_eq!(judge(&r, &p).0, Verdict::Allow, "{verb}");
        }
    }

    #[test]
    fn near_miss_names_the_method_blocked_rule() {
        let p = policy(
            r#"{"rules":[
                {"name":"connection:github","match":{"host":"api.github.com","method":["GET"]},"verdict":"allow"},
                {"name":"other","match":{"host":"example.com"},"verdict":"allow"}]}"#,
        );
        let mut post = req();
        post.host = "api.github.com".into();
        assert_eq!(judge(&post, &p), (Verdict::Deny, None));
        let (rule, methods) = method_near_miss(&post, &p).unwrap();
        assert_eq!(rule, "connection:github");
        assert_eq!(methods, vec!["GET"]);
        // Wrong host entirely: no near-miss to report.
        let mut elsewhere = req();
        elsewhere.host = "evil.com".into();
        assert!(method_near_miss(&elsewhere, &p).is_none());
        // Out-of-scope rules stay out of the hint too.
        let scoped = policy(
            r#"{"rules":[{"name":"w1","match":{"host":"api.github.com","method":"GET"},"verdict":"allow","scope":"org/w1"}]}"#,
        );
        let mut other_worker = post.clone();
        other_worker.worker = Some("org/w2".into());
        assert!(method_near_miss(&other_worker, &scoped).is_none());
    }

    #[test]
    fn mcp_tool_globs() {
        let p = policy(
            r#"{"rules":[{"name":"ro","match":{"mcp":{"method":"tools/call","tool":["get_*","list_*"]}},"verdict":"allow"}]}"#,
        );
        let mut r = req();
        r.mcp = Some(Mcp {
            method: "tools/call".into(),
            tool: Some("get_issue".into()),
            batch: false,
        });
        assert_eq!(judge(&r, &p).0, Verdict::Allow);
        r.mcp = Some(Mcp {
            method: "tools/call".into(),
            tool: Some("create_pr".into()),
            batch: false,
        });
        assert_eq!(judge(&r, &p).0, Verdict::Deny);
        r.mcp = None;
        assert_eq!(judge(&r, &p).0, Verdict::Deny);
    }

    #[test]
    fn rule_scope_is_ancestor_filtered() {
        // A rule scoped to org/w1 must not govern a request from org/w2.
        let p = policy(
            r#"{"rules":[{"name":"w1-only","match":{"host":"chatgpt.com"},"verdict":"allow","scope":"org/w1"}]}"#,
        );
        let mut r = req();
        r.worker = Some("org/w1".into());
        assert_eq!(judge(&r, &p).0, Verdict::Allow);
        r.worker = Some("org/w2".into());
        assert_eq!(judge(&r, &p).0, Verdict::Deny); // out of scope → no rule → default deny
                                                    // An org-scoped rule governs any worker.
        let org = policy(
            r#"{"rules":[{"name":"all","match":{"host":"chatgpt.com"},"verdict":"allow","scope":"org"}]}"#,
        );
        assert_eq!(judge(&r, &org).0, Verdict::Allow);
    }

    #[test]
    fn host_and_glob_helpers() {
        assert!(host_matches("*", "anything.com"));
        assert!(host_matches("*.githubcopilot.com", "api.githubcopilot.com"));
        assert!(host_matches("*.githubcopilot.com", "githubcopilot.com"));
        assert!(!host_matches(
            "*.githubcopilot.com",
            "githubcopilot.com.evil.com"
        ));
        assert!(!host_matches("chatgpt.com", "evil.chatgpt.com"));
        assert!(glob_matches("get_*", "get_issue"));
        assert!(!glob_matches("get_*", "set_issue"));
        assert!(!glob_matches("get_*", "xget_issue"));
    }
}
