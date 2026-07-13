//! `roster server vault sync` — import an existing pi login into the vault
//! (the shortcut alternative to `vault connect`) — and `vault ls`, which shows
//! credential names and types only. Never values.

use crate::util::BErr;
use serde_json::Value;
use std::fs;
use std::path::Path;

pub fn run() -> Result<(), BErr> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let src = Path::new(&home).join(".pi/agent/auth.json");
    let text = fs::read_to_string(&src).map_err(|_| format!("no pi auth to sync from at {}", src.display()))?;
    let auth: Value = serde_json::from_str(&text)?;
    let entries = auth.as_object().ok_or("pi auth.json is not an object")?;

    let vault = crate::credential::vault::vault_dir();
    fs::create_dir_all(&vault)?;
    let mut names = Vec::new();
    for (name, cred) in entries {
        let path = vault.join(format!("{name}.json"));
        fs::write(&path, format!("{}\n", serde_json::to_string_pretty(cred)?))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
        names.push(name.clone());
    }
    println!("vault synced from pi auth: {}", names.join(", "));
    println!("(stored at {} — off the box mount; the gateway injects these in transit)", vault.display());
    Ok(())
}

/// Credential names, types, and freshness — never values.
pub fn ls(json: bool) -> Result<(), BErr> {
    let vault = crate::credential::vault::vault_dir();
    let mut rows: Vec<(String, String, String)> = Vec::new();
    if let Ok(entries) = fs::read_dir(&vault) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let kind = fs::read_to_string(&path)
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(String::from))
                .unwrap_or_else(|| "?".into());
            let updated = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|t| {
                    let dt: time::OffsetDateTime = t.into();
                    dt.format(&time::format_description::well_known::Rfc3339)
                        .map(|s| s.chars().take(16).collect::<String>().replace('T', " "))
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            rows.push((name, kind, updated));
        }
    }
    rows.sort();
    if json {
        let rows: Vec<Value> = rows
            .iter()
            .map(|(name, kind, updated)| serde_json::json!({"name": name, "type": kind, "updated": updated}))
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("the vault is empty — create a credential: roster server vault connect <provider>");
        return Ok(());
    }
    println!("{:<16}  {:<10}  UPDATED (UTC)", "CREDENTIAL", "TYPE");
    for (name, kind, updated) in rows {
        println!("{name:<16}  {kind:<10}  {updated}");
    }
    Ok(())
}
