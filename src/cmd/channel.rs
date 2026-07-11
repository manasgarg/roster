//! `roster channel` — manage the trust designation of Discord channels. A
//! trusted channel's non-admin participants are trusted (can administer, and the
//! worker replies there without a gate); an untrusted channel's are content-only.
//! The Discord `/channel trust|untrust` slash command maps here.
//!
//!   roster channel ls
//!   roster channel trust|untrust <channel_id>
//!   roster channel mode <channel_id> all|mention

use crate::discord;

type BErr = Box<dyn std::error::Error>;

pub fn run(args: &[String]) -> Result<(), BErr> {
    match args.first().map(String::as_str).unwrap_or("ls") {
        "ls" | "list" => {
            let map = discord::channel_settings_all();
            if map.is_empty() {
                println!("no channels configured (untrusted, mode=all by default)");
            } else {
                for (id, s) in map {
                    println!(
                        "{id}  {}  mode={}  memory={}  inferred={}  kinds={}  retention={}  memory-notes={}  memory-chars={}",
                        if s.trusted { "trusted  " } else { "untrusted" },
                        s.mode,
                        if s.memory_enabled { "on" } else { "off" },
                        if s.memory_inferred_auto { "auto" } else { "review" },
                        s.memory_allowed_kinds.as_ref().map(|v| v.join(",")).unwrap_or_else(|| "default".into()),
                        s.memory_retention_days.map(|n| format!("{n}d")).unwrap_or_else(|| "default".into()),
                        s.memory_recall_max_notes.map(|n| n.to_string()).unwrap_or_else(|| "default".into()),
                        s.memory_recall_char_budget.map(|n| n.to_string()).unwrap_or_else(|| "default".into()),
                    );
                }
            }
            Ok(())
        }
        "trust" => {
            let id = args.get(1).ok_or("usage: roster channel trust <channel_id>")?;
            discord::set_channel_trust(id, true);
            println!("channel {id} → trusted");
            Ok(())
        }
        "untrust" => {
            let id = args.get(1).ok_or("usage: roster channel untrust <channel_id>")?;
            discord::set_channel_trust(id, false);
            println!("channel {id} → untrusted");
            Ok(())
        }
        "mode" => {
            let id = args.get(1).ok_or("usage: roster channel mode <channel_id> all|mention")?;
            let mode = args.get(2).map(String::as_str).ok_or("usage: roster channel mode <channel_id> all|mention")?;
            if mode != "all" && mode != "mention" {
                return Err("mode must be \"all\" or \"mention\"".into());
            }
            discord::set_channel_mode(id, mode);
            println!("channel {id} → mode {mode}");
            Ok(())
        }
        "memory" => {
            let id = args.get(1).ok_or("usage: roster channel memory <channel_id> on|off")?;
            let enabled = match args.get(2).map(String::as_str) {
                Some("on") => true,
                Some("off") => false,
                _ => return Err("memory must be \"on\" or \"off\"".into()),
            };
            discord::set_channel_memory(id, enabled);
            println!("channel {id} → memory {}", if enabled { "on" } else { "off" });
            Ok(())
        }
        "memory-budget" => {
            let id = args.get(1).ok_or("usage: roster channel memory-budget <channel_id> <notes|default> <chars|default>")?;
            let parse = |value: Option<&String>| -> Result<Option<usize>, BErr> {
                match value.map(String::as_str) {
                    Some("default") => Ok(None),
                    Some(v) => Ok(Some(v.parse::<usize>().map_err(|_| "memory budget wants positive integers or default")?)),
                    None => Err("memory budget needs both note and character limits".into()),
                }
            };
            let notes = parse(args.get(2))?;
            let chars = parse(args.get(3))?;
            if notes == Some(0) || chars == Some(0) {
                return Err("memory budgets must be positive".into());
            }
            discord::set_channel_memory_budget(id, notes, chars);
            println!("channel {id} → memory budget notes={} chars={}", notes.map(|n| n.to_string()).unwrap_or_else(|| "default".into()), chars.map(|n| n.to_string()).unwrap_or_else(|| "default".into()));
            Ok(())
        }
        "memory-inferred" => {
            let id = args.get(1).ok_or("usage: roster channel memory-inferred <channel_id> auto|review")?;
            let enabled = match args.get(2).map(String::as_str) {
                Some("auto") => true,
                Some("review") => false,
                _ => return Err("memory-inferred must be \"auto\" or \"review\"".into()),
            };
            discord::set_channel_memory_inferred_auto(id, enabled);
            println!("channel {id} → inferred memory {}", if enabled { "auto" } else { "review" });
            Ok(())
        }
        "memory-kinds" => {
            let id = args.get(1).ok_or("usage: roster channel memory-kinds <channel_id> default|<comma-separated-kinds>")?;
            let value = args.get(2).ok_or("memory-kinds needs default or a comma-separated list")?;
            let kinds = parse_memory_kinds(value)?;
            discord::set_channel_memory_allowed_kinds(id, kinds.clone());
            println!("channel {id} → memory kinds {}", kinds.map(|v| v.join(",")).unwrap_or_else(|| "default".into()));
            Ok(())
        }
        "memory-retention" => {
            let id = args.get(1).ok_or("usage: roster channel memory-retention <channel_id> default|<days>")?;
            let days = match args.get(2).map(String::as_str) {
                Some("default") => None,
                Some(value) => Some(value.parse::<u64>().ok().filter(|n| *n > 0).ok_or("retention days must be positive")?),
                None => return Err("memory-retention needs default or a number of days".into()),
            };
            discord::set_channel_memory_retention_days(id, days);
            println!("channel {id} → memory retention {}", days.map(|n| format!("{n} days")).unwrap_or_else(|| "default".into()));
            Ok(())
        }
        other => Err(format!("unknown channel subcommand \"{other}\" (try: ls, trust, untrust, mode, memory, memory-budget, memory-inferred, memory-kinds, memory-retention)").into()),
    }
}

pub fn parse_memory_kinds(value: &str) -> Result<Option<Vec<String>>, BErr> {
    if value == "default" {
        return Ok(None);
    }
    let allowed = crate::memory::SUPPORTED_MEMORY_KINDS;
    let kinds: Vec<String> = value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    if kinds.is_empty() || kinds.iter().any(|kind| !allowed.contains(&kind.as_str())) {
        return Err(format!("memory kinds must be a comma-separated subset of {}", allowed.join(",")).into());
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
