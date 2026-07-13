//! Schedule triggers — the proactive wake-up (§3.5). A trigger fires on its
//! interval by FILING a task (never doing work inline); the supervisor then
//! dispatches it like any other, and D6 budget-gates it at dispatch. Compiled
//! from worker.toml `[[trigger]]` into runs/compiled/triggers.json; last-fired
//! times persist in queue/.trigger-state.json so a restart doesn't double-fire.

use crate::paths;
use crate::util::now_ms;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct Trigger {
    /// Short worker name (the box's `--worker`).
    pub worker: String,
    /// Interval like "every 30s", "10m", "1h", "24h", "7d".
    pub schedule: String,
    pub prompt: String,
    #[serde(default = "default_ceiling")]
    pub ceiling_min: f64,
}

fn default_ceiling() -> f64 {
    20.0
}

pub fn load() -> Vec<Trigger> {
    crate::config::snapshot()
        .map(|c| {
            c.triggers
                .iter()
                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Parse an interval to milliseconds. "every " prefix optional; unit s/m/h/d.
pub fn parse_interval(s: &str) -> Option<i64> {
    let s = s.trim().strip_prefix("every").unwrap_or(s).trim();
    let (num, unit) = s.split_at(s.find(|c: char| c.is_alphabetic())?);
    let n: i64 = num.trim().parse().ok()?;
    let mult = match unit.trim() {
        "s" | "sec" | "secs" => 1_000,
        "m" | "min" | "mins" => 60_000,
        "h" | "hr" | "hrs" => 3_600_000,
        "d" | "day" | "days" => 86_400_000,
        _ => return None,
    };
    Some(n * mult)
}

fn state_path() -> PathBuf {
    paths::trigger_state_file()
}

fn load_state() -> HashMap<String, i64> {
    std::fs::read_to_string(state_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(state: &HashMap<String, i64>) {
    let path = state_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string_pretty(state) {
        let _ = std::fs::write(path, text);
    }
}

fn key(t: &Trigger) -> String {
    format!("{}|{}|{}", t.worker, t.schedule, t.prompt)
}

/// File a proactive task for every trigger whose interval has elapsed. Returns
/// how many fired. Idempotent within an interval (last-fired is persisted).
pub fn fire() -> usize {
    let triggers = load();
    if triggers.is_empty() {
        return 0;
    }
    let mut state = load_state();
    let now = now_ms();
    let mut fired = 0;
    for t in &triggers {
        let Some(interval) = parse_interval(&t.schedule) else {
            eprintln!("trigger for {}: unparseable schedule \"{}\" — skipped", t.worker, t.schedule);
            continue;
        };
        let last = state.get(&key(t)).copied().unwrap_or(0);
        if now - last >= interval {
            match crate::work::queue::create(&t.worker, &t.prompt, "schedule", true, t.ceiling_min, "append", Value::Null, None, None) {
                Ok(task) => {
                    state.insert(key(t), now);
                    fired += 1;
                    eprintln!("trigger → queued {} for {} ({})", task.id, t.worker, t.schedule);
                }
                Err(e) => eprintln!("trigger for {}: could not queue: {e}", t.worker),
            }
        }
    }
    save_state(&state);
    fired
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_intervals() {
        assert_eq!(parse_interval("every 30s"), Some(30_000));
        assert_eq!(parse_interval("10m"), Some(600_000));
        assert_eq!(parse_interval("every 24h"), Some(86_400_000));
        assert_eq!(parse_interval("7d"), Some(7 * 86_400_000));
        assert_eq!(parse_interval("nonsense"), None);
    }
}
