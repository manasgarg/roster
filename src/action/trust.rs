//! The trust ladder: per `(worker, intent)`, decide whether a proposed action
//! runs automatically or waits for a human. T0 (the default) gates every
//! irreversible; the admin promotes an intent to `auto`, optionally narrowed by
//! a predicate over the action payload (e.g. recipient `*@ourco.com`). Trust is
//! earned upward and revocable. See docs/actions-and-trust.md.

use crate::gateway::judge::glob_matches;
use crate::gateway::scope::applies;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

fn org_scope() -> String {
    "org".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct TrustRule {
    #[serde(default = "org_scope")]
    pub scope: String,
    pub intent: String,
    /// Payload predicates: field → glob. All must hold for the rule to apply.
    #[serde(default, rename = "match")]
    pub predicate: HashMap<String, String>,
    /// "auto" | "gate" | "earned"
    pub level: String,
    /// For level "earned": promote to auto after this many clean approvals.
    #[serde(default)]
    pub after: Option<u32>,
}

/// The trust level for this proposal: the first applicable admin rule decides,
/// else the action grant's default (T0 = "gate"). `executed`/`denied` are this
/// (worker, intent)'s gate history, for the "earned" ladder — auto once enough
/// have been approved with no reversal (a denial resets the privilege).
pub fn evaluate(
    worker: &str,
    intent: &str,
    payload: &Value,
    default_level: &str,
    rules: &[TrustRule],
    executed: u32,
    denied: u32,
) -> String {
    for r in rules {
        if applies(&r.scope, worker) && r.intent == intent && predicate_matches(&r.predicate, payload)
        {
            if r.level == "earned" {
                let after = r.after.unwrap_or(5);
                return if denied == 0 && executed >= after {
                    "auto".into()
                } else {
                    "gate".into()
                };
            }
            return r.level.clone();
        }
    }
    default_level.to_string()
}

/// A predicate holds when every field matches its glob. A list field (e.g. email
/// `to`) matches only if *all* elements match — so one external recipient in an
/// otherwise-internal list falls through to a gate, never silently auto-sends.
fn predicate_matches(preds: &HashMap<String, String>, payload: &Value) -> bool {
    preds.iter().all(|(field, pat)| match payload.get(field) {
        Some(Value::String(s)) => matches_value(pat, s),
        Some(Value::Array(a)) => {
            !a.is_empty()
                && a.iter()
                    .all(|e| e.as_str().map(|s| matches_value(pat, s)).unwrap_or(false))
        }
        _ => false,
    })
}

/// Match a glob against a value, tolerating a mailbox display name: both the raw
/// string and its bracketed address are tried, so `manasgarg@gmail.com` matches
/// `Manas Garg <manasgarg@gmail.com>`.
fn matches_value(pat: &str, s: &str) -> bool {
    glob_matches(pat, s.trim()) || glob_matches(pat, bare_address(s))
}

/// The address inside `<...>` if present, else the trimmed string.
fn bare_address(s: &str) -> &str {
    match (s.rfind('<'), s.rfind('>')) {
        (Some(a), Some(b)) if b > a + 1 => s[a + 1..b].trim(),
        _ => s.trim(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rules() -> Vec<TrustRule> {
        vec![TrustRule {
            scope: "org".into(),
            intent: "email-send".into(),
            predicate: [("to".to_string(), "*@ourco.com".to_string())].into(),
            level: "auto".into(),
            after: None,
        }]
    }

    #[test]
    fn internal_recipients_auto_external_gates() {
        let rs = rules();
        // all internal → auto
        assert_eq!(
            evaluate(
                "org/yuko",
                "email-send",
                &json!({"to":["a@ourco.com","b@ourco.com"]}),
                "gate",
                &rs,
                0,
                0
            ),
            "auto"
        );
        // one external → predicate fails → default gate
        assert_eq!(
            evaluate(
                "org/yuko",
                "email-send",
                &json!({"to":["a@ourco.com","x@evil.com"]}),
                "gate",
                &rs,
                0,
                0
            ),
            "gate"
        );
    }

    #[test]
    fn default_when_no_rule_matches() {
        assert_eq!(
            evaluate(
                "org/yuko",
                "discord-send",
                &json!({}),
                "gate",
                &rules(),
                0,
                0
            ),
            "gate"
        );
        assert_eq!(
            evaluate("org/yuko", "message-user", &json!({}), "auto", &[], 0, 0),
            "auto"
        );
    }

    #[test]
    fn matches_recipient_with_display_name() {
        let rs = vec![TrustRule {
            scope: "org".into(),
            intent: "email-send".into(),
            predicate: [("to".to_string(), "manasgarg@gmail.com".to_string())].into(),
            level: "auto".into(),
            after: None,
        }];
        // plain address, and "Name <addr>" form, both auto
        assert_eq!(
            evaluate(
                "org/yuko",
                "email-send",
                &json!({"to":["manasgarg@gmail.com"]}),
                "gate",
                &rs,
                0,
                0
            ),
            "auto"
        );
        assert_eq!(
            evaluate(
                "org/yuko",
                "email-send",
                &json!({"to":["Manas Garg <manasgarg@gmail.com>"]}),
                "gate",
                &rs,
                0,
                0
            ),
            "auto"
        );
        // a different address still gates
        assert_eq!(
            evaluate(
                "org/yuko",
                "email-send",
                &json!({"to":["Someone <other@gmail.com>"]}),
                "gate",
                &rs,
                0,
                0
            ),
            "gate"
        );
    }

    #[test]
    fn scope_gates_out_of_scope_workers() {
        let rs = vec![TrustRule {
            scope: "org/w1".into(),
            intent: "email-send".into(),
            predicate: HashMap::new(),
            level: "auto".into(),
            after: None,
        }];
        assert_eq!(
            evaluate("org/w1", "email-send", &json!({}), "gate", &rs, 0, 0),
            "auto"
        );
        assert_eq!(
            evaluate("org/w2", "email-send", &json!({}), "gate", &rs, 0, 0),
            "gate"
        );
    }

    #[test]
    fn earned_promotes_after_threshold_and_resets_on_denial() {
        let rs = vec![TrustRule {
            scope: "org".into(),
            intent: "email-send".into(),
            predicate: HashMap::new(),
            level: "earned".into(),
            after: Some(3),
        }];
        let p = json!({"to":["x@out.com"]});
        assert_eq!(
            evaluate("org/yuko", "email-send", &p, "gate", &rs, 0, 0),
            "gate"
        ); // none yet
        assert_eq!(
            evaluate("org/yuko", "email-send", &p, "gate", &rs, 2, 0),
            "gate"
        ); // under threshold
        assert_eq!(
            evaluate("org/yuko", "email-send", &p, "gate", &rs, 3, 0),
            "auto"
        ); // reached
        assert_eq!(
            evaluate("org/yuko", "email-send", &p, "gate", &rs, 9, 1),
            "gate"
        ); // a denial revokes it
    }
}
