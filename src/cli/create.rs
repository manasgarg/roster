//! `roster worker init <name>` — scaffold a minimal worker spec.

use crate::paths;
use std::fs;

pub fn run(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let ok = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && name.as_bytes()[0] != b'-';
    if !ok {
        return Err(
            format!("worker name must be lowercase letters/numbers/hyphens: \"{name}\"").into(),
        );
    }
    if name == "org" {
        return Err(
            "\"org\" is reserved — it names the org scope and the fleet-wide grant edge".into(),
        );
    }

    let dir = paths::worker_dir(name);
    let path = dir.join("worker.toml");
    let identity = dir.join("identity.md");
    let knowledge_ready = crate::worker::knowledge::repo_path(name).is_ok();
    // Refuse only a *fully* initialized worker; a half-finished init (files
    // written but knowledge init crashed, or vice versa) must be re-runnable to
    // completion instead of being permanently blocked by "already exists".
    if path.exists() && identity.exists() && knowledge_ready {
        return Err(format!(
            "worker \"{name}\" already exists and is fully set up at {} — edit its files directly",
            dir.display()
        )
        .into());
    }
    fs::create_dir_all(&dir)?;

    // Create-if-missing throughout, so a re-run never clobbers an admin's edits.
    if !path.exists() {
        fs::write(
            &path,
            format!("# Worker spec — ADMIN-ONLY. Overlays org.toml at scope \"org/{name}\".\nname = \"{name}\"\n"),
        )?;
        println!("created {}", path.display());
    } else {
        println!("kept    {}", path.display());
    }

    // A deliberately minimal identity: a name, and the fact of being a digital
    // worker. Everything else is shaped later — by the admin editing this file,
    // or by the worker proposing changes (gated, D10). Operating principles
    // live in the runtime policy, not here.
    if !identity.exists() {
        fs::write(
            &identity,
            format!(
                "# {name}\n\n\
                 Your name is {name}. You're a worker — a colleague made of software,\n\
                 not a human. That's all that's fixed about you. The rest of who you are\n\
                 takes shape through the work you do and the people you do it with.\n"
            ),
        )?;
        println!("created {}", identity.display());
    } else {
        println!("kept    {}", identity.display());
    }

    if knowledge_ready {
        println!("kept    knowledge repository");
    } else {
        let knowledge_commit = crate::worker::knowledge::initialize(name).map_err(|error| {
            format!(
                "worker files are in place, but its knowledge repository could not be initialized \
                 (re-run `roster worker init {name}` after fixing): {error}"
            )
        })?;
        println!("initialized knowledge at {knowledge_commit}");
    }
    println!("edit them anytime — config loads live (roster server validate checks it)");
    println!(
        "\nnext: roster talk {name}   (or file work: roster worker task add {name} \"<prompt>\")"
    );
    if !crate::run::boxed::model_credentials_available() {
        println!(
            "note: no model credential yet — {name} can't think until one is connected: \
             roster connection add anthropic  (or openai-codex)"
        );
    }
    Ok(())
}
