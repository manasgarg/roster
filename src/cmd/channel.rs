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
                    println!("{id}  {}  mode={}", if s.trusted { "trusted  " } else { "untrusted" }, s.mode);
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
        other => Err(format!("unknown channel subcommand \"{other}\" (try: ls, trust, untrust, mode)").into()),
    }
}
