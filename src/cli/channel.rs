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
        "{:<22}  {:<28}  {:<9}  MODE",
        "CHANNEL", "WHERE", "TRUST"
    );
    for (id, s) in map {
        println!(
            "{:<22}  {:<28}  {:<9}  {}",
            id,
            describe(&id),
            if s.trusted { "trusted" } else { "untrusted" },
            s.mode,
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
    discord::set_channel_trust(channel_id, trusted)?;
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
    println!("channel {channel_id}  ({})\n", describe(channel_id));
    println!("{:<18}  {:<34}  CURRENT", "KEY", "VALUES");
    let rows = [("mode", "all | mention", s.mode.clone())];
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
            discord::set_channel_mode(channel_id, value)?;
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



#[cfg(test)]
mod tests {
    use super::*;

}
