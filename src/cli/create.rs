//! `impyard imp init <name>` — scaffold a minimal imp spec.

use crate::paths;
use std::fs;

pub fn run(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let ok = !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && name.as_bytes()[0] != b'-';
    if !ok {
        return Err(format!("imp name must be lowercase letters/numbers/hyphens: \"{name}\"").into());
    }

    let dir = paths::imp_dir(name);
    let path = dir.join("imp.toml");
    if path.exists() {
        return Err(format!("imp \"{name}\" already exists at {}", path.display()).into());
    }
    fs::create_dir_all(&dir)?;
    fs::write(
        &path,
        format!("# Imp spec — ADMIN-ONLY. Overlays org.toml at scope \"org/{name}\".\nname = \"{name}\"\n"),
    )?;

    // A deliberately minimal identity: a name, and the fact of being a digital
    // imp. Everything else is shaped later — by the admin editing this file,
    // or by the imp proposing changes (gated, D10). Operating principles
    // live in the runtime policy, not here.
    let identity = dir.join("identity.md");
    fs::write(
        &identity,
        format!(
            "# {name}\n\n\
             Your name is {name}. You're an imp — a colleague made of software,\n\
             not a human. That's all that's fixed about you. The rest of who you are\n\
             takes shape through the work you do and the people you do it with.\n"
        ),
    )?;

    println!("created {}", path.display());
    println!("created {}", identity.display());
    let knowledge_commit = crate::imp::knowledge::initialize(name).map_err(|error| {
        format!(
            "imp files were created, but its knowledge repository could not be initialized: {error}"
        )
    })?;
    println!("initialized knowledge at {knowledge_commit}");
    println!("edit them, then run: impyard server deploy");
    Ok(())
}
