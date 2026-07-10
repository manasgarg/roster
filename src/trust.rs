//! The trust ladder: per `(worker, intent)`, decide whether a proposed action
//! runs automatically or waits for a human. T0 (the default) gates every
//! irreversible; the owner promotes an intent to `auto`, optionally narrowed by
//! a predicate over the action payload (e.g. recipient `*@ourco.com`). Trust is
//! earned upward and revocable. See docs/supervisor-spec.md.

use crate::judge::glob_matches;
use crate::scope::applies;
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
    /// "auto" | "gate"
    pub level: String,
}

/// The trust level for this proposal: the first applicable owner rule's level,
/// else the action grant's default (T0 = "gate"). "auto" means execute now.
pub fn evaluate(worker: &str, intent: &str, payload: &Value, default_level: &str, rules: &[TrustRule]) -> String {
    for r in rules {
        if applies(&r.scope, worker) && r.intent == intent && predicate_matches(&r.predicate, payload) {
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
        Some(Value::String(s)) => glob_matches(pat, s),
        Some(Value::Array(a)) => !a.is_empty() && a.iter().all(|e| e.as_str().map(|s| glob_matches(pat, s)).unwrap_or(false)),
        _ => false,
    })
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
        }]
    }

    #[test]
    fn internal_recipients_auto_external_gates() {
        let rs = rules();
        // all internal → auto
        assert_eq!(evaluate("org/yuko", "email-send", &json!({"to":["a@ourco.com","b@ourco.com"]}), "gate", &rs), "auto");
        // one external → predicate fails → default gate
        assert_eq!(evaluate("org/yuko", "email-send", &json!({"to":["a@ourco.com","x@evil.com"]}), "gate", &rs), "gate");
    }

    #[test]
    fn default_when_no_rule_matches() {
        assert_eq!(evaluate("org/yuko", "discord-send", &json!({}), "gate", &rules()), "gate");
        assert_eq!(evaluate("org/yuko", "message-user", &json!({}), "auto", &[]), "auto");
    }

    #[test]
    fn scope_gates_out_of_scope_workers() {
        let rs = vec![TrustRule { scope: "org/w1".into(), intent: "email-send".into(), predicate: HashMap::new(), level: "auto".into() }];
        assert_eq!(evaluate("org/w1", "email-send", &json!({}), "gate", &rs), "auto");
        assert_eq!(evaluate("org/w2", "email-send", &json!({}), "gate", &rs), "gate");
    }
}
