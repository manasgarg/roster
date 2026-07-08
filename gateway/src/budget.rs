//! Metering: map each governed call to a spend vector across owner-defined
//! currencies, via CEL expressions over the call context. B1 — computes and
//! logs spend; enforcement (limits + ledger) is B2. See docs/budget-spec.md.

use crate::schema::GovernedRequest;
use crate::util::root;
use cel_interpreter::{Context, Program, Value as Cel};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct BudgetPolicy {
    #[serde(default)]
    #[allow(dead_code)] // scope: used when worker identity lands (B4)
    pub scope: String,
    #[serde(default)]
    #[allow(dead_code)] // currencies: a declaration; meters/limits reference these
    pub currencies: Vec<String>,
    #[serde(default)]
    pub vars: Value,
    #[serde(default)]
    pub meters: Vec<Meter>,
    #[serde(default)]
    pub limits: Vec<Limit>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Meter {
    #[serde(rename = "match")]
    pub match_expr: String,
    #[serde(default)]
    pub spend: HashMap<String, String>,
}

/// A cap on cumulative draw of one currency, at a namespace scope, per window.
#[derive(Debug, Clone, Deserialize)]
pub struct Limit {
    pub scope: String,
    pub currency: String,
    pub window: Window,
    pub max: f64,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Window {
    Minute,
    Hour,
    Day,
    Month,
}

impl Window {
    /// Window length in ms. `month` is approximated as 30 days for now (true
    /// calendar months are a later refinement).
    pub fn ms(self) -> i64 {
        match self {
            Window::Minute => 60_000,
            Window::Hour => 3_600_000,
            Window::Day => 86_400_000,
            Window::Month => 30 * 86_400_000,
        }
    }
    /// Start of the current window (calendar-aligned via epoch modulo).
    pub fn start(self, now: i64) -> i64 {
        now - now.rem_euclid(self.ms())
    }
    pub fn label(self) -> &'static str {
        match self {
            Window::Minute => "minute",
            Window::Hour => "hour",
            Window::Day => "day",
            Window::Month => "month",
        }
    }
}

/// Read `policies/budget.json` fresh each call (owner edits are live). Absent or
/// unparseable ⇒ no meters (no spend recorded; the judge still governs).
pub fn load_budget() -> BudgetPolicy {
    let path = root().join("runs").join("compiled").join("budget.json");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<BudgetPolicy>(&s).ok())
        .unwrap_or_default()
}

fn eval(program_src: &str, ctx: &Context) -> Option<Cel> {
    Program::compile(program_src)
        .ok()
        .and_then(|p| p.execute(ctx).ok())
}

fn as_bool(v: &Cel) -> bool {
    matches!(v, Cel::Bool(true))
}

fn as_f64(v: &Cel) -> Option<f64> {
    match v {
        Cel::Float(f) => Some(*f),
        Cel::Int(i) => Some(*i as f64),
        Cel::UInt(u) => Some(*u as f64),
        _ => None,
    }
}

/// Compute the currency spend a call drew. `response` is `{}` until B3 lifts
/// response-derived signals (tokens); count/derived-from-request currencies work
/// now. Meters whose `match` CEL is true contribute their `spend` expressions.
pub fn compute_spend(
    gr: &GovernedRequest,
    verdict: &str,
    rule: Option<&str>,
    response: &Value,
    policy: &BudgetPolicy,
) -> HashMap<String, f64> {
    let request = json!({
        "protocol": gr.protocol, "method": gr.method, "host": gr.host, "port": gr.port,
        "path": gr.path, "query": gr.query, "bodyBytes": gr.body_size,
        "mcp": gr.mcp.as_ref().map(|m| json!({"method": m.method, "tool": m.tool})),
    });
    let decision = json!({ "verdict": verdict, "rule": rule });

    let mut ctx = Context::default();
    let _ = ctx.add_variable("request", request);
    let _ = ctx.add_variable("response", response.clone());
    let _ = ctx.add_variable("decision", decision);
    let _ = ctx.add_variable("subject", gr.worker.clone().unwrap_or_default());
    let _ = ctx.add_variable("vars", policy.vars.clone());

    let mut out: HashMap<String, f64> = HashMap::new();
    for meter in &policy.meters {
        match eval(&meter.match_expr, &ctx) {
            Some(v) if as_bool(&v) => {}
            _ => continue,
        }
        for (currency, expr) in &meter.spend {
            if let Some(n) = eval(expr, &ctx).as_ref().and_then(as_f64) {
                *out.entry(currency.clone()).or_insert(0.0) += n;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn gr(host: &str) -> GovernedRequest {
        GovernedRequest {
            worker: None,
            protocol: "https".into(),
            method: "POST".into(),
            host: host.into(),
            port: 443,
            path: "/x".into(),
            query: String::new(),
            headers: HashMap::new(),
            body_size: 10,
            mcp: None,
        }
    }

    fn policy(s: &str) -> BudgetPolicy {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn count_meter_by_rule() {
        let p = policy(
            r#"{"currencies":["model_calls","usd"],"vars":{"price":{"call":0.01}},
                "meters":[{"match":"decision.rule == 'model-api'","spend":{"model_calls":"1","usd":"vars.price.call"}}]}"#,
        );
        let spend = compute_spend(&gr("chatgpt.com"), "allow", Some("model-api"), &json!({}), &p);
        assert_eq!(spend.get("model_calls"), Some(&1.0));
        assert_eq!(spend.get("usd"), Some(&0.01));
    }

    #[test]
    fn non_matching_rule_draws_nothing() {
        let p = policy(r#"{"meters":[{"match":"decision.rule == 'web-search'","spend":{"searches":"1"}}]}"#);
        let spend = compute_spend(&gr("chatgpt.com"), "allow", Some("model-api"), &json!({}), &p);
        assert!(spend.is_empty());
    }

    #[test]
    fn can_meter_on_request_fields() {
        let p = policy(r#"{"meters":[{"match":"request.host == 'chatgpt.com'","spend":{"bytes":"request.bodyBytes"}}]}"#);
        let spend = compute_spend(&gr("chatgpt.com"), "allow", Some("model-api"), &json!({}), &p);
        assert_eq!(spend.get("bytes"), Some(&10.0));
    }
}
