//! `roster channel` — manage the trust designation of Discord channels. A
//! trusted channel's non-admin participants are trusted (can administer, and the
//! worker replies there without a gate); an untrusted channel's are content-only.
//! The Discord `/channel trust|untrust` slash command maps here.
//!
//!   roster channel ls
//!   roster channel trust <channel_id>
//!   roster channel untrust <channel_id>

use crate::discord;

type BErr = Box<dyn std::error::Error>;

pub fn run(args: &[String]) -> Result<(), BErr> {
    match args.first().map(String::as_str).unwrap_or("ls") {
        "ls" | "list" => {
            let map = discord::trust_designations();
            if map.is_empty() {
                println!("no channels designated (all untrusted by default)");
            } else {
                for (id, trusted) in map {
                    println!("{id}  {}", if trusted { "trusted" } else { "untrusted" });
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
        other => Err(format!("unknown channel subcommand \"{other}\" (try: ls, trust, untrust)").into()),
    }
}
