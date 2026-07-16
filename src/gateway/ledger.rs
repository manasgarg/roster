//! The budget ledger: in-memory currency counters the gateway checks and
//! debits, backed by an append-only `audit/usage.jsonl` (source of truth,
//! rehydrated on boot). See docs/gateway.md.

use crate::gateway::budget::{Limit, Window};
use crate::util::{now_ms, now_rfc3339};
use serde_json::json;
use std::collections::HashMap;
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
use crate::gateway::scope::applies as scope_applies;

fn usage_path() -> std::path::PathBuf {
    // Unit tests exercise debit(); they must never append to the real audit
    // log of a deployment on the same machine.
    #[cfg(test)]
    return std::env::temp_dir().join(format!("roster-test-usage-{}.jsonl", std::process::id()));
    #[cfg(not(test))]
    crate::paths::usage_log()
}

/// A budget refusal: why, and when the window resets (for `Retry-After`).
pub struct Refusal {
    pub reason: String,
    pub retry_after_secs: i64,
}

/// First limit this spend would breach at the current balance, if any. Semantics:
/// the call that crosses the line completes; the next one is refused.
fn breach(
    c: &HashMap<String, Counter>,
    subject: &str,
    spend: &HashMap<String, f64>,
    limits: &[Limit],
    now: i64,
) -> Option<Refusal> {
    for limit in limits {
        if !scope_applies(&limit.scope, subject) || !spend.contains_key(&limit.currency) {
            continue;
        }
        let used = match c.get(&key(&limit.scope, &limit.currency, limit.window)) {
            Some(ct) if ct.window_start == limit.window.start(now) => ct.used,
            _ => 0.0,
        };
        if used >= limit.max {
            let reset_ms = limit.window.start(now) + limit.window.ms() - now;
            return Some(Refusal {
                reason: format!(
                    "{} over {} cap ({:.4}/{:.4})",
                    limit.currency,
                    limit.window.label(),
                    used,
                    limit.max
                ),
                retry_after_secs: (reset_ms + 999) / 1000,
            });
        }
    }
    None
}

/// Bump the in-memory counters for every window that limits a drawn currency.
fn commit_counters(
    c: &mut HashMap<String, Counter>,
    subject: &str,
    spend: &HashMap<String, f64>,
    limits: &[Limit],
    now: i64,
) {
    for (currency, amount) in spend {
        for limit in limits
            .iter()
            .filter(|l| scope_applies(&l.scope, subject) && &l.currency == currency)
        {
            let ws = limit.window.start(now);
            let ct = c
                .entry(key(&limit.scope, currency, limit.window))
                .or_insert(Counter {
                    window_start: ws,
                    used: 0.0,
                });
            if ct.window_start != ws {
                ct.window_start = ws;
                ct.used = 0.0;
            }
            ct.used += amount;
        }
    }
}

/// Atomically enforce and reserve budget in one lock hold: if no limit is
/// breached at the current balance, commit the spend to the counters and return
/// None; otherwise commit nothing and return the refusal. This closes the
/// check-then-debit gap — without it, N concurrent requests at used=max-1 all
/// pass a separate check before any debit lands, blowing the cap by N-1. The
/// caller persists the reserved spend with `record_usage` afterward.
pub fn reserve(
    subject: &str,
    spend: &HashMap<String, f64>,
    limits: &[Limit],
    now: i64,
) -> Option<Refusal> {
    let mut c = counters().lock().unwrap();
    if let Some(refusal) = breach(&c, subject, spend, limits, now) {
        return Some(refusal);
    }
    commit_counters(&mut c, subject, spend, limits, now);
    None
}

/// Is the subject already at/over any limit that applies to it (any currency)?
/// The supervisor uses this for D6's soft stop: proactive work is skipped when
/// the worker is tapped out; admin-filed/chat work is never checked here.
pub fn over_any_limit(subject: &str, limits: &[Limit], now: i64) -> Option<String> {
    let c = counters().lock().unwrap();
    for limit in limits {
        if !scope_applies(&limit.scope, subject) {
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
/// Append the drawn currencies to usage.jsonl — the durable audit + rehydrate
/// source — after the spend has been reserved/committed in memory.
pub fn record_usage(subject: &str, spend: &HashMap<String, f64>, now: i64) {
    // Serialize the append across processes: without it, two concurrent writers
    // interleave their bytes into a corrupt line that rehydrate() silently drops
    // on the next boot — losing spend the worker already made.
    let path = usage_path();
    let _lock = match crate::statefile::FileLock::acquire_path(&usage_lock_path()) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("ledger: could not lock usage log ({e}); spend not durably recorded");
            return;
        }
    };
    for (currency, amount) in spend {
        let line = json!({"ts": now_rfc3339(), "ts_ms": now, "subject": subject, "currency": currency, "amount": amount});
        if let Err(e) = crate::statefile::append_line(&path, &line.to_string()) {
            eprintln!("ledger: could not append usage record ({e})");
        }
    }
}

fn usage_lock_path() -> std::path::PathBuf {
    usage_path().with_extension("jsonl.lock")
}

/// Current balance per limit — for inspection (`server status`, `worker show`).
/// A fresh CLI process must call `rehydrate()` first.
pub fn balances(limits: &[Limit], now: i64) -> Vec<(Limit, f64)> {
    let c = counters().lock().unwrap();
    limits
        .iter()
        .map(|l| {
            let used = match c.get(&key(&l.scope, &l.currency, l.window)) {
                Some(ct) if ct.window_start == l.window.start(now) => ct.used,
                _ => 0.0,
            };
            (l.clone(), used)
        })
        .collect()
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
        for limit in limits
            .iter()
            .filter(|l| scope_applies(&l.scope, subj) && l.currency == cur)
        {
            let ws = limit.window.start(now);
            if ts_ms >= ws {
                let ct = c
                    .entry(key(&limit.scope, cur, limit.window))
                    .or_insert(Counter {
                        window_start: ws,
                        used: 0.0,
                    });
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
        Limit {
            scope: "org".into(),
            currency: currency.into(),
            window,
            max,
        }
    }

    #[test]
    fn reserve_does_not_overshoot_under_contention() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        // Unique currency so this counter is isolated from other tests.
        let limits = Arc::new(vec![limit("reserve_race_cur", Window::Hour, 100.0)]);
        let spend: HashMap<String, f64> = [("reserve_race_cur".to_string(), 1.0)].into();
        let now = now_ms();
        let allowed = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let limits = limits.clone();
                let spend = spend.clone();
                let allowed = allowed.clone();
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        // 400 attempts, cap 100: a separate check/debit would
                        // let many extra through; reserve must not.
                        if reserve("org", &spend, &limits, now).is_none() {
                            allowed.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(allowed.load(Ordering::SeqCst), 100, "reserve overshot the cap");
    }

    #[test]
    fn reserve_denies_only_after_the_cap_is_reached() {
        let limits = vec![limit("calls_test", Window::Hour, 2.0)];
        let spend: HashMap<String, f64> = [("calls_test".to_string(), 1.0)].into();
        let now = now_ms();
        assert!(reserve("org", &spend, &limits, now).is_none()); // used = 1
        assert!(reserve("org", &spend, &limits, now).is_none()); // used = 2 (crossed)
        assert!(reserve("org", &spend, &limits, now).is_some()); // now refused
    }

    #[test]
    fn unlimited_currency_never_denies() {
        let limits = vec![limit("calls_test", Window::Hour, 1.0)];
        let spend: HashMap<String, f64> = [("other_cur".to_string(), 5.0)].into();
        assert!(reserve("org", &spend, &limits, now_ms()).is_none());
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
        assert!(reserve("org/w1", &spend, &limits, now).is_none()); // org counter = 1
        assert!(reserve("org/w2", &spend, &limits, now).is_none()); // org counter = 2 (crossed)
                                                                    // a third call under org is now refused
        assert!(reserve("org/w3", &spend, &limits, now).is_some());
    }
}
