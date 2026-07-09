//! `roster create <name>` — scaffold a minimal worker spec.

use crate::util::root;
use std::fs;

pub fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let name = args.first().ok_or("create needs a worker name: roster create <name>")?;
    let ok = !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && name.as_bytes()[0] != b'-';
    if !ok {
        return Err(format!("worker name must be lowercase letters/numbers/hyphens: \"{name}\"").into());
    }

    let dir = root().join("workers").join(name);
    let path = dir.join("worker.toml");
    if path.exists() {
        return Err(format!("worker \"{name}\" already exists at {}", path.display()).into());
    }
    fs::create_dir_all(&dir)?;
    fs::write(
        &path,
        format!("# Worker spec — OWNER-ONLY. Overlays org.toml at scope \"org/{name}\".\nname = \"{name}\"\n"),
    )?;
    println!("created {}", path.display());
    println!("edit it, then run: roster deploy");
    Ok(())
}
