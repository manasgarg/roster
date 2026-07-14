//! `impyard imp memory` — admin inspection and repair of scoped
//! interaction memory. (Module name is the legacy storage name; the physical
//! `notes/` → `memory/` migration finishes via `compact`.)

use crate::util::BErr;
use crate::imp::memory::{self, MemoryScope};

pub fn compact(imp: &str) -> Result<(), BErr> {
    crate::imp::require_imp(imp)?;
    let kept = memory::compact(imp).map_err(|e| -> BErr { e.into() })?;
    println!("compacted {imp} memory; kept {kept} live notes");
    Ok(())
}

pub fn ls(imp: &str, scope: Option<&str>, scope_id: Option<&str>) -> Result<(), BErr> {
    crate::imp::require_imp(imp)?;
    let scope = match scope {
        None => None,
        Some("imp") => Some(MemoryScope::Imp),
        Some("channel") => Some(MemoryScope::Channel),
        Some("user") => Some(MemoryScope::User),
        Some(other) => return Err(format!("unknown memory scope \"{other}\" (imp, channel, user)").into()),
    };
    let notes: Vec<_> = memory::list(imp)
        .into_iter()
        .filter(|n| scope.as_ref().map(|s| &n.scope == s).unwrap_or(true))
        .filter(|n| {
            scope_id
                .map(|id| n.scope_id.as_deref() == Some(id))
                .unwrap_or(true)
        })
        .collect();
    if notes.is_empty() {
        println!("no memories for {imp}");
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

pub fn show(imp: &str, id: &str) -> Result<(), BErr> {
    crate::imp::require_imp(imp)?;
    let note = memory::find(imp, id).ok_or_else(|| format!("no such memory {id}"))?;
    println!("{}", serde_json::to_string_pretty(&note)?);
    Ok(())
}

pub fn mutate(op: &str, imp: &str, id: &str) -> Result<(), BErr> {
    crate::imp::require_imp(imp)?;
    memory::admin_mutate(imp, op, id, None).map_err(|e| -> BErr { e.into() })?;
    println!("memory {id} → {op}");
    Ok(())
}

pub fn correct(imp: &str, id: &str, replacement: &str) -> Result<(), BErr> {
    crate::imp::require_imp(imp)?;
    if replacement.trim().is_empty() {
        return Err("correct needs the replacement text".into());
    }
    memory::admin_mutate(imp, "correct", id, Some(replacement)).map_err(|e| -> BErr { e.into() })?;
    println!("memory {id} → correct");
    Ok(())
}
