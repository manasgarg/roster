//! The budget ledger: in-memory currency counters the gateway checks and
//! debits, backed by an append-only `runs/usage.jsonl` (source of truth,
//! rehydrated on boot). B2 — enforcement on un-falsifiable count currencies,
//! org-global scope. See docs/budget-spec.md.

use crate::budget::{Limit, Window};
use crate::util::{now_ms, now_rfc3339, root};
use serde_json::json;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

struct Counter {
    window_start: i64,
    used: f64,
}

fn counters() -> &'static Mutex<HashMap<String, Counter>> {
    static C: OnceLock<Mutex<HashMap<String, Counter>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key(scope: &str, currency: &str, window: Window) -> String {
    format!("{scope}|{currency}|{}", window.label())
}

// Ancestor scope matching, shared with the judge. Counters are keyed by the
// *limit's* scope, so all subjects under a scope roll up into its counter.
use crate::scope::applies as scope_applies;

fn usage_path() -> std::path::PathBuf {
    root().join("runs").join("usage.jsonl")
}

/// Would this call breach any limit? Checks the CURRENT balance (semantics: the
/// call that crosses the line completes; the next one is refused). Returns the
/// first breached limit's reason, or None.
pub fn check(subject: &str, spend: &HashMap<String, f64>, limits: &[Limit], now: i64) -> Option<String> {
    let c = counters().lock().unwrap();
    for limit in limits {
        if !scope_applies(&limit.scope, subject) || !spend.contains_key(&limit.currency) {
            continue;
        }
        let used = match c.get(&key(&limit.scope, &limit.currency, limit.window)) {
            Some(ct) if ct.window_start == limit.window.start(now) => ct.used,
            _ => 0.0,
        };
        if used >= limit.max {
            return Some(format!(
                "{} over {} cap ({:.4}/{:.4})",
                limit.currency,
                limit.window.label(),
                used,
                limit.max
            ));
        }
    }
    None
}

/// Record spend: bump the in-memory counters (per window that limits the
/// currency) and append every drawn currency to usage.jsonl (audit + rehydrate
/// source, incl. unlimited currencies).
pub fn debit(subject: &str, spend: &HashMap<String, f64>, limits: &[Limit], now: i64) {
    {
        let mut c = counters().lock().unwrap();
        for (currency, amount) in spend {
            for limit in limits.iter().filter(|l| scope_applies(&l.scope, subject) && &l.currency == currency) {
                let ws = limit.window.start(now);
                let ct = c
                    .entry(key(&limit.scope, currency, limit.window))
                    .or_insert(Counter { window_start: ws, used: 0.0 });
                if ct.window_start != ws {
                    ct.window_start = ws;
                    ct.used = 0.0;
                }
                ct.used += amount;
            }
        }
    }
    let path = usage_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        for (currency, amount) in spend {
            let line = json!({"ts": now_rfc3339(), "ts_ms": now, "subject": subject, "currency": currency, "amount": amount});
            let _ = writeln!(f, "{line}");
        }
    }
}

/// Rebuild the in-memory counters from the current window's usage on boot, so a
/// restart doesn't reset budgets (the ops-scar law: durable, not in-memory).
pub fn rehydrate(limits: &[Limit]) {
    let text = match std::fs::read_to_string(usage_path()) {
        Ok(t) => t,
        Err(_) => return,
    };
    let now = now_ms();
    let mut c = counters().lock().unwrap();
    for line in text.lines() {
        let ev: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let subj = ev.get("subject").and_then(|v| v.as_str()).unwrap_or("");
        let cur = ev.get("currency").and_then(|v| v.as_str()).unwrap_or("");
        let amt = ev.get("amount").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let ts_ms = ev.get("ts_ms").and_then(|v| v.as_i64()).unwrap_or(0);
        for limit in limits.iter().filter(|l| scope_applies(&l.scope, subj) && l.currency == cur) {
            let ws = limit.window.start(now);
            if ts_ms >= ws {
                let ct = c
                    .entry(key(&limit.scope, cur, limit.window))
                    .or_insert(Counter { window_start: ws, used: 0.0 });
                if ct.window_start != ws {
                    ct.window_start = ws;
                    ct.used = 0.0;
                }
                ct.used += amt;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limit(currency: &str, window: Window, max: f64) -> Limit {
        Limit { scope: "org".into(), currency: currency.into(), window, max }
    }

    #[test]
    fn check_denies_only_after_the_cap_is_reached() {
        let limits = vec![limit("calls_test", Window::Hour, 2.0)];
        let spend: HashMap<String, f64> = [("calls_test".to_string(), 1.0)].into();
        let now = now_ms();
        // fresh: under cap → allowed
        assert!(check("org", &spend, &limits, now).is_none());
        debit("org", &spend, &limits, now); // used = 1
        assert!(check("org", &spend, &limits, now).is_none());
        debit("org", &spend, &limits, now); // used = 2 (this call still went; it crossed)
        // now at cap → next call refused
        assert!(check("org", &spend, &limits, now).is_some());
    }

    #[test]
    fn unlimited_currency_never_denies() {
        let limits = vec![limit("calls_test", Window::Hour, 1.0)];
        let spend: HashMap<String, f64> = [("other_cur".to_string(), 5.0)].into();
        assert!(check("org", &spend, &limits, now_ms()).is_none());
    }

    #[test]
    fn scope_matching_is_ancestor_based() {
        assert!(scope_applies("org", "org"));
        assert!(scope_applies("org", "org/yuko"));
        assert!(scope_applies("org", "org/team/yuko"));
        assert!(scope_applies("org/team", "org/team/yuko"));
        assert!(!scope_applies("org/team", "org/other"));
        assert!(!scope_applies("org/yuko", "org")); // a worker limit doesn't govern the org
    }

    #[test]
    fn org_aggregate_rolls_up_across_subjects() {
        // One org-wide cap; two different workers draw against the same counter.
        let limits = vec![limit("rollup_cur", Window::Hour, 2.0)];
        let spend: HashMap<String, f64> = [("rollup_cur".to_string(), 1.0)].into();
        let now = now_ms();
        debit("org/w1", &spend, &limits, now); // org counter = 1
        debit("org/w2", &spend, &limits, now); // org counter = 2 (crossed)
        // a third call from any subject under org is now refused
        assert!(check("org/w3", &spend, &limits, now).is_some());
    }
}
