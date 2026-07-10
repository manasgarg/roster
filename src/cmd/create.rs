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

    // A starter identity — the worker's fixed self, read into every run.
    // Owner-authored; the worker can only propose changes (admin-gated, D10).
    // Channel-specific purpose lives per channel (channels/<id>/purpose.md).
    let identity = dir.join("identity.md");
    fs::write(
        &identity,
        format!(
            "# Identity — {name}\n\n\
             You are {name}, a digital worker for your owner. This is your fixed identity and\n\
             standing rules — the same in every channel; it leads every task you run.\n\n\
             ## Who you are\n\n\
             <Persona, tone, and what you're generally for.>\n\n\
             ## Standing rules\n\n\
             - Prefer proposing over acting; consequential actions are reviewed.\n\
             - <Add rules specific to this worker.>\n"
        ),
    )?;

    println!("created {}", path.display());
    println!("created {}", identity.display());
    println!("edit them, then run: roster deploy");
    Ok(())
}
