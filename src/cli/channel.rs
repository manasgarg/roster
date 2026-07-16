//! `roster server channel` — manage channel edges. `trust`/`untrust` is the
//! security designation (a trusted channel's non-admin participants may
//! administer, and the worker replies there without a gate; an untrusted
//! channel's are content-only). Everything else is tuning, through one uniform
//! `set <id> <key> <value>`. The Discord `/channel …` slash commands map here.

use crate::channel::discord::{self, ChannelSettings};
use crate::util::BErr;

const SET_KEYS: &str =
    "mode, memory, memory-inferred, memory-kinds, memory-retention, memory-notes, memory-chars";

/// A channel's human identity: what the listener learned ("#general @ rototo",
/// "DM with jane"), or derived for terminal channels, or the bare id.
pub fn describe(channel_id: &str) -> String {
    if let Some(meta) = discord::channel_meta(channel_id) {
        let platform = meta.get("platform").and_then(|v| v.as_str()).unwrap_or("?");
        let name = meta.get("name").and_then(|v| v.as_str()).unwrap_or("");
        return match meta.get("server").and_then(|v| v.as_str()) {
            Some(server) if !server.is_empty() => format!("{platform} #{name} @ {server}"),
            _ => format!("{platform} {name}"),
        };
    }
    if let Some(rest) = channel_id.strip_prefix("term-") {
        if let Some((user, worker)) = rest.rsplit_once('-') {
            return format!("terminal {user} ↔ {worker}");
        }
    }
    "-".into()
}

pub fn ls(json: bool) -> Result<(), BErr> {
    let map = discord::channel_settings_all();
    if json {
        println!("{}", serde_json::to_string_pretty(&map)?);
        return Ok(());
    }
    if map.is_empty() {
        println!("no channels configured — channels appear when a worker binds one ([channels] in its worker.toml) or when you talk to a worker in the terminal (roster talk <name>)");
        println!("until configured, every channel defaults to untrusted, mode=all");
        return Ok(());
    }
    println!(
        "{:<22}  {:<28}  {:<9}  {:<7}  MEMORY",
        "CHANNEL", "WHERE", "TRUST", "MODE"
    );
    for (id, s) in map {
        println!(
            "{:<22}  {:<28}  {:<9}  {:<7}  {}",
            id,
            describe(&id),
            if s.trusted { "trusted" } else { "untrusted" },
            s.mode,
            memory_summary(&s),
        );
    }
    println!("\ndetails: roster server channel show <id>");
    Ok(())
}

pub fn show(channel_id: &str) -> Result<(), BErr> {
    let map = discord::channel_settings_all();
    let s = map.get(channel_id).cloned().unwrap_or_default();
    let configured = map.contains_key(channel_id);
    println!(
        "channel   {channel_id}{}",
        if configured {
            ""
        } else {
            "   (not configured — defaults)"
        }
    );
    let described = describe(channel_id);
    if described != "-" {
        println!("where     {described}");
    }
    println!(
        "trust     {}",
        if s.trusted { "trusted" } else { "untrusted" }
    );
    println!(
        "mode      {:<10} (all = every message wakes the worker; mention = @mention/DM only)",
        s.mode
    );
    println!("memory    {}", if s.memory_enabled { "on" } else { "off" });
    println!(
        "  inferred   {:<10} (auto = inferred notes apply; review = they gate)",
        if s.memory_inferred_auto {
            "auto"
        } else {
            "review"
        }
    );
    println!(
        "  kinds      {}",
        s.memory_allowed_kinds
            .as_ref()
            .map(|v| v.join(","))
            .unwrap_or_else(|| "default".into())
    );
    println!(
        "  retention  {}",
        s.memory_retention_days
            .map(|n| format!("{n} days"))
            .unwrap_or_else(|| "default".into())
    );
    println!(
        "  recall     {} notes / {} chars",
        s.memory_recall_max_notes
            .map(|n| n.to_string())
            .unwrap_or_else(|| "default".into()),
        s.memory_recall_char_budget
            .map(|n| n.to_string())
            .unwrap_or_else(|| "default".into())
    );
    Ok(())
}

pub fn set_trust(channel_id: &str, trusted: bool) -> Result<(), BErr> {
    // Trust is a standing grant; a typo'd id must not pass silently.
    if !discord::channel_settings_all().contains_key(channel_id) && describe(channel_id) == "-" {
        eprintln!(
            "warning: no known channel matches \"{channel_id}\" — the designation is recorded \
             and applies if such a channel appears (known channels: roster server channel ls)"
        );
    }
    discord::set_channel_trust(channel_id, trusted);
    println!(
        "channel {channel_id} → {}",
        if trusted { "trusted" } else { "untrusted" }
    );
    Ok(())
}

/// Bare `channel set <id>`: show every key, its allowed values, and the
/// channel's current value — the same pattern as `connection add` without a
/// service.
pub fn set_help(channel_id: &str) -> Result<(), BErr> {
    let s = current_settings(channel_id);
    let current_kinds = s
        .memory_allowed_kinds
        .as_ref()
        .map(|v| v.join(","))
        .unwrap_or_else(|| "default".into());
    let current_retention = s
        .memory_retention_days
        .map(|n| n.to_string())
        .unwrap_or_else(|| "default".into());
    let current_notes = s
        .memory_recall_max_notes
        .map(|n| n.to_string())
        .unwrap_or_else(|| "default".into());
    let current_chars = s
        .memory_recall_char_budget
        .map(|n| n.to_string())
        .unwrap_or_else(|| "default".into());
    println!("channel {channel_id}  ({})\n", describe(channel_id));
    println!("{:<18}  {:<34}  CURRENT", "KEY", "VALUES");
    let rows = [
        ("mode", "all | mention", s.mode.clone()),
        (
            "memory",
            "on | off",
            if s.memory_enabled { "on" } else { "off" }.to_string(),
        ),
        (
            "memory-inferred",
            "auto | review",
            if s.memory_inferred_auto { "auto" } else { "review" }.to_string(),
        ),
        ("memory-kinds", "default | kind,kind,…", current_kinds),
        ("memory-retention", "default | days", current_retention),
        ("memory-notes", "default | max notes recalled", current_notes),
        ("memory-chars", "default | max chars recalled", current_chars),
    ];
    for (key, values, current) in rows {
        println!("{key:<18}  {values:<34}  {current}");
    }
    println!("\nset one: roster server channel set {channel_id} <key> <value>");
    Ok(())
}

pub fn set(channel_id: &str, key: &str, value: &str) -> Result<(), BErr> {
    match key {
        "mode" => {
            if value != "all" && value != "mention" {
                return Err("mode must be \"all\" or \"mention\"".into());
            }
            discord::set_channel_mode(channel_id, value);
        }
        "memory" => {
            let enabled = on_off(value)?;
            discord::set_channel_memory(channel_id, enabled);
        }
        "memory-inferred" => {
            let auto = match value {
                "auto" => true,
                "review" => false,
                _ => return Err("memory-inferred must be \"auto\" or \"review\"".into()),
            };
            discord::set_channel_memory_inferred_auto(channel_id, auto);
        }
        "memory-kinds" => {
            let kinds = parse_memory_kinds(value)?;
            discord::set_channel_memory_allowed_kinds(channel_id, kinds);
        }
        "memory-retention" => {
            let days = match value {
                "default" => None,
                v => Some(
                    v.parse::<u64>()
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or("retention wants a positive number of days, or default")?,
                ),
            };
            discord::set_channel_memory_retention_days(channel_id, days);
        }
        // The recall budget is stored as one (notes, chars) pair; setting one
        // half keeps the other as currently configured.
        "memory-notes" => {
            let notes = budget_value(value)?;
            let current = current_settings(channel_id);
            discord::set_channel_memory_budget(
                channel_id,
                notes,
                current.memory_recall_char_budget,
            );
        }
        "memory-chars" => {
            let chars = budget_value(value)?;
            let current = current_settings(channel_id);
            discord::set_channel_memory_budget(channel_id, current.memory_recall_max_notes, chars);
        }
        other => {
            return Err(format!("unknown channel setting \"{other}\" (keys: {SET_KEYS})").into())
        }
    }
    println!("channel {channel_id} → {key} {value}");
    Ok(())
}

fn current_settings(channel_id: &str) -> ChannelSettings {
    discord::channel_settings_all()
        .get(channel_id)
        .cloned()
        .unwrap_or_default()
}

fn memory_summary(s: &ChannelSettings) -> String {
    if !s.memory_enabled {
        return "off".into();
    }
    format!(
        "on ({}, kinds={}, retention={}, recall={}/{})",
        if s.memory_inferred_auto {
            "auto"
        } else {
            "review"
        },
        s.memory_allowed_kinds
            .as_ref()
            .map(|v| v.join(","))
            .unwrap_or_else(|| "default".into()),
        s.memory_retention_days
            .map(|n| format!("{n}d"))
            .unwrap_or_else(|| "default".into()),
        s.memory_recall_max_notes
            .map(|n| n.to_string())
            .unwrap_or_else(|| "default".into()),
        s.memory_recall_char_budget
            .map(|n| n.to_string())
            .unwrap_or_else(|| "default".into()),
    )
}

fn on_off(value: &str) -> Result<bool, BErr> {
    match value {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err("value must be \"on\" or \"off\"".into()),
    }
}

fn budget_value(value: &str) -> Result<Option<usize>, BErr> {
    match value {
        "default" => Ok(None),
        v => v
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .map(Some)
            .ok_or_else(|| "recall budgets want a positive integer, or default".into()),
    }
}

pub fn parse_memory_kinds(value: &str) -> Result<Option<Vec<String>>, BErr> {
    if value == "default" {
        return Ok(None);
    }
    let allowed = crate::worker::memory::SUPPORTED_MEMORY_KINDS;
    let kinds: Vec<String> = value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    if kinds.is_empty() || kinds.iter().any(|kind| !allowed.contains(&kind.as_str())) {
        return Err(format!(
            "memory kinds must be a comma-separated subset of {}",
            allowed.join(",")
        )
        .into());
    }
    Ok(Some(kinds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_kinds_are_validated() {
        assert!(parse_memory_kinds("default").unwrap().is_none());
        assert!(parse_memory_kinds("research").is_err());
        assert_eq!(
            parse_memory_kinds("fact,interaction").unwrap().unwrap(),
            vec!["fact", "interaction"]
        );
        assert!(parse_memory_kinds("fact,secrets").is_err());
    }
}
