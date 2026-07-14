//! `roster credential ls` — credential names, types, and freshness. Never values.

use crate::util::BErr;
use serde_json::Value;
use std::fs;

pub fn ls(json: bool) -> Result<(), BErr> {
    let vault = crate::credential::vault::vault_dir();
    let mut rows: Vec<(String, String, String)> = Vec::new();
    if let Ok(entries) = fs::read_dir(&vault) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
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
            .map(|(name, kind, updated)| {
                serde_json::json!({"name": name, "type": kind, "updated": updated})
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no credentials — create one: roster credential add <provider>");
        return Ok(());
    }
    println!("{:<16}  {:<10}  UPDATED (UTC)", "CREDENTIAL", "TYPE");
    for (name, kind, updated) in rows {
        println!("{name:<16}  {kind:<10}  {updated}");
    }
    Ok(())
}
