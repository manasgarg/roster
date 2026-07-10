//! `roster listen --worker <name>` — run the Discord gateway client (inbound).
//! Dials out to Discord with the bot token from the vault, discovers the channels
//! it can see, and turns messages into content tasks for the worker.

use crate::discord;

type BErr = Box<dyn std::error::Error>;

pub async fn run(args: &[String]) -> Result<(), BErr> {
    let mut worker = String::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--worker" => {
                worker = args.get(i + 1).cloned().ok_or("--worker wants a name")?;
                i += 2;
            }
            other => return Err(format!("unknown listen flag \"{other}\"").into()),
        }
    }
    if worker.is_empty() {
        return Err("listen needs --worker <name>".into());
    }

    let cred = crate::vault::get_credential("discord").ok_or("no discord credential — run: roster connect discord")?;
    let token = cred.get("token").and_then(|v| v.as_str()).ok_or("discord credential has no token")?.to_string();

    eprintln!("roster listen — connecting the Discord gateway for {worker}");
    discord::run_gateway(&worker, &token).await;
    Ok(())
}
