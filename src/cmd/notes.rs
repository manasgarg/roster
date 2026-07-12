//! `roster worker memory` — admin inspection and repair of scoped
//! interaction memory. (Module name is the legacy storage name; the physical
//! `notes/` → `memory/` migration finishes via `compact`.)

use super::BErr;
use crate::memory::{self, MemoryScope};

pub fn compact(worker: &str) -> Result<(), BErr> {
    super::require_worker(worker)?;
    let kept = memory::compact(worker).map_err(|e| -> BErr { e.into() })?;
    println!("compacted {worker} memory; kept {kept} live notes");
    Ok(())
}

pub fn ls(worker: &str, scope: Option<&str>, scope_id: Option<&str>) -> Result<(), BErr> {
    super::require_worker(worker)?;
    let scope = match scope {
        None => None,
        Some("worker") => Some(MemoryScope::Worker),
        Some("channel") => Some(MemoryScope::Channel),
        Some("user") => Some(MemoryScope::User),
        Some(other) => return Err(format!("unknown memory scope \"{other}\" (worker, channel, user)").into()),
    };
    let notes: Vec<_> = memory::list(worker)
        .into_iter()
        .filter(|n| scope.as_ref().map(|s| &n.scope == s).unwrap_or(true))
        .filter(|n| {
            scope_id
                .map(|id| n.scope_id.as_deref() == Some(id))
                .unwrap_or(true)
        })
        .collect();
    if notes.is_empty() {
        println!("no memories for {worker}");
        return Ok(());
    }
    println!(
        "{:<18}  {:<9}  {:<20}  {:<11}  {:<10}  NOTE",
        "ID", "SCOPE", "SUBJECT", "KIND", "STATUS"
    );
    for n in notes {
        let mut text = n.note.replace('\n', " ");
        if text.chars().count() > 70 {
            text = text.chars().take(69).collect::<String>() + "…";
        }
        println!(
            "{:<18}  {:<9}  {:<20}  {:<11}  {:<10}  {}{}",
            n.id,
            n.scope.as_str(),
            n.scope_id.as_deref().unwrap_or("-"),
            n.kind,
            n.status(),
            if n.pinned { "pinned: " } else { "" },
            text
        );
    }
    Ok(())
}

pub fn show(worker: &str, id: &str) -> Result<(), BErr> {
    super::require_worker(worker)?;
    let note = memory::find(worker, id).ok_or_else(|| format!("no such memory {id}"))?;
    println!("{}", serde_json::to_string_pretty(&note)?);
    Ok(())
}

pub fn mutate(op: &str, worker: &str, id: &str) -> Result<(), BErr> {
    super::require_worker(worker)?;
    memory::admin_mutate(worker, op, id, None).map_err(|e| -> BErr { e.into() })?;
    println!("memory {id} → {op}");
    Ok(())
}

pub fn correct(worker: &str, id: &str, replacement: &str) -> Result<(), BErr> {
    super::require_worker(worker)?;
    if replacement.trim().is_empty() {
        return Err("correct needs the replacement text".into());
    }
    memory::admin_mutate(worker, "correct", id, Some(replacement)).map_err(|e| -> BErr { e.into() })?;
    println!("memory {id} → correct");
    Ok(())
}
