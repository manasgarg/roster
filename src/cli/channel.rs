//! `roster channel` — manage channels (conversations). `trust`/`untrust` is the
//! security designation (a trusted channel's non-admin participants may
//! administer, and the worker replies there without a gate; an untrusted
//! channel's are content-only). Everything else is tuning, through one uniform
//! `set <id> <key> <value>`. The Discord `/channel …` slash commands map here.

use crate::channel::discord::{self, ChannelSettings};
use crate::util::BErr;

const SET_KEYS: &str =
    "mode, memory, memory-inferred, memory-kinds, memory-retention, memory-notes, memory-chars";

/// A channel's human identity: its linked surfaces, what the listener
/// learned ("#general @ rototo", "DM with jane"), or derived for terminal
/// channels, or the bare id.
pub fn describe(channel_id: &str) -> String {
    let members = crate::channel::links::surfaces_of(channel_id);
    if members.len() > 1 {
        return format!("linked: {}", members.join(", "));
    }
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
    println!("{:<22}  {:<28}  {:<9}  MODE", "CHANNEL", "WHERE", "TRUST");
    for (id, s) in map {
        println!(
            "{:<22}  {:<28}  {:<9}  {}",
            id,
            describe(&id),
            if s.trusted { "trusted" } else { "untrusted" },
            s.mode,
        );
    }
    println!("\ndetails: roster channel show <id>");
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
    let members = crate::channel::links::surfaces_of(channel_id);
    if members.len() > 1 {
        println!("surfaces");
        for m in &members {
            println!("  {m:<20}  {}", describe(m));
        }
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
    // A surface id resolves to its linked channel — trust belongs to the
    // conversation, and saying so teaches the membership.
    let logical = crate::channel::links::logical_of(channel_id);
    if logical != channel_id {
        println!(
            "surface {channel_id} belongs to channel {logical}; {} {logical}",
            if trusted { "trusting" } else { "untrusting" }
        );
    }
    // Trust is a standing grant; a typo'd id must not pass silently.
    if !discord::channel_settings_all().contains_key(&logical) && describe(&logical) == "-" {
        eprintln!(
            "warning: no known channel matches \"{logical}\" — the designation is recorded \
             and applies if such a channel appears (known channels: roster channel ls)"
        );
    }
    discord::set_channel_trust(&logical, trusted)?;
    println!(
        "channel {logical} → {}",
        if trusted { "trusted" } else { "untrusted" }
    );
    Ok(())
}

/// `roster channel link <name> <surface>…` — declare that surfaces are one
/// conversation. The operator's act, from the authenticated CLI; the merge
/// of history/purpose/settings happens once, here.
pub fn link(name: &str, surfaces: &[String]) -> Result<(), BErr> {
    crate::channel::links::link(name, surfaces)?;
    let members = crate::channel::links::surfaces_of(name);
    println!("channel {name} — linked surfaces: {}", members.join(", "));
    println!(
        "one conversation now: shared history, purpose, trust, and channel store; \
         replies go to whichever surface each message arrives on"
    );
    Ok(())
}

pub fn unlink(surface_id: &str) -> Result<(), BErr> {
    let name = crate::channel::links::unlink(surface_id)?;
    println!(
        "unlinked {surface_id} from channel {name} — the channel and its material stay \
         with the remaining members; {surface_id} starts fresh as its own channel"
    );
    Ok(())
}

/// `roster channel rm <id>` — delete a channel: the settings entry is
/// dropped (back to the defaults: untrusted, mode=all), and the channel's
/// record (history, meta, cursors) and per-worker channel stores are
/// archived under the data trash, mirroring `worker rm`. A linked id fails
/// closed — unlink the members first, then remove the pieces.
pub fn rm(channel_id: &str, yes: bool) -> Result<(), BErr> {
    use crate::paths;
    if !crate::channel::links::safe_component(channel_id) {
        return Err(format!("invalid channel id \"{channel_id}\"").into());
    }
    if let Some(name) = crate::channel::links::in_registry(channel_id) {
        return Err(format!(
            "\"{channel_id}\" is part of linked channel \"{name}\" — unlink its surfaces first \
             (roster channel show {name}), then remove them individually"
        )
        .into());
    }

    let record = paths::channel_dir(channel_id);
    let configured = discord::channel_settings_all().contains_key(channel_id);
    let store_owners: Vec<String> = crate::worker::names()
        .into_iter()
        .filter(|w| paths::worker_channel_store_dir(w, channel_id).is_dir())
        .collect();
    if !record.is_dir() && !configured && store_owners.is_empty() {
        return Err(
            format!("no channel \"{channel_id}\" (known channels: roster channel ls)").into(),
        );
    }

    let stamp: String = crate::util::now_rfc3339()
        .chars()
        .take(19)
        .map(|c| if c == ':' { '-' } else { c })
        .collect();
    let trash = paths::data_root()
        .join("trash")
        .join(format!("channel-{channel_id}-{stamp}"));

    if !yes {
        let interactive = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
        if !interactive {
            return Err(format!(
                "removing a channel needs confirmation — re-run with --yes: roster channel rm {channel_id} --yes"
            )
            .into());
        }
        let described = describe(channel_id);
        if described != "-" {
            println!("channel {channel_id}  ({described})");
        }
        println!(
            "this drops the channel's settings and archives its history and stores under:\n  {}",
            trash.display()
        );
        let answer = crate::credential::connect::ask("delete? [y/N] ")?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            return Err("not confirmed — nothing was removed".into());
        }
    }

    if discord::remove_channel_settings(channel_id)? {
        println!("dropped settings for {channel_id} (trust and mode revert to the defaults)");
    }
    if record.is_dir() {
        std::fs::create_dir_all(&trash)?;
        std::fs::rename(&record, trash.join("record"))?;
        println!("archived record → {}", trash.join("record").display());
    }
    for worker in &store_owners {
        std::fs::create_dir_all(&trash)?;
        let dest = trash.join(format!("store-{worker}"));
        std::fs::rename(paths::worker_channel_store_dir(worker, channel_id), &dest)?;
        println!("archived {worker}'s channel store → {}", dest.display());
    }
    println!("restore: move the directories back; delete for good: remove the trash entry");
    println!(
        "note: if a listener still shares this conversation, the channel reappears \
         (with default settings) on its next message"
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
    println!("\nset one: roster channel set {channel_id} <key> <value>");
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
