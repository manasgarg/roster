//! `roster init` — initialize a deployment: the XDG config/data/state roots
//! and a starter org.toml. Idempotent; never overwrites an existing file.

use crate::paths;
use crate::util::BErr;

const STARTER_ORG: &str = r#"# Roster org config — ADMIN-ONLY. Applies to every worker (scope "org");
# per-worker overlays live in workers/<name>/worker.toml. Config loads live:
# check edits with `roster server validate`.

# pi + the box extensions are baked into the roster-box image. Developers
# iterating on them can mount a checkout over the baked engine instead:
# [engine]
# dir = "/path/to/roster"

# Egress grants shared by all workers, e.g.:
# [[grant]]
# name = "web-fetch"
# match = { host = "*", method = "GET" }
# verdict = "allow"

# Actions workers may propose ([[action]]), the trust ladder ([[trust]]),
# and budgets ([budget] + [[budget.limit]]) — full reference and worked
# examples: docs/configuration.md in the roster repo
# (https://github.com/manasgarg/roster).
"#;

fn dirs() -> [(&'static str, std::path::PathBuf); 10] {
    [
        ("config", paths::config_root()),
        ("config/workers", paths::workers_dir()),
        ("data", paths::data_root()),
        ("data/vault", paths::vault_dir()),
        ("data/workers", paths::workers_data_dir()),
        ("data/channels", paths::channels_dir()),
        ("state", paths::state_root()),
        ("state/runs", paths::runs_dir()),
        ("state/locks", paths::locks_dir()),
        ("state/identity", paths::identity_dir()),
    ]
}

/// Bring the deployment roots into existence, quietly — called on every
/// command, so there is no first-run ceremony: install → `roster worker
/// init <name>` → go. Idempotent, cheap on the happy path, and it never
/// overwrites an existing file.
pub fn ensure() -> Result<(), BErr> {
    for (_, dir) in dirs() {
        if !dir.exists() {
            std::fs::create_dir_all(&dir)?;
        }
    }
    let org = paths::org_file();
    if !org.exists() {
        std::fs::write(&org, STARTER_ORG)?;
        eprintln!(
            "roster: initialized a fresh deployment (org.toml at {})",
            org.display()
        );
    }
    Ok(())
}

/// The loud explicit form — harmless, kept for muscle memory and scripts.
pub fn run() -> Result<(), BErr> {
    for (label, dir) in dirs() {
        if !dir.exists() {
            std::fs::create_dir_all(&dir)?;
            println!("created {label:<15} {}", dir.display());
        }
    }
    let org = paths::org_file();
    if org.exists() {
        println!("kept    {:<15} {}", "org.toml", org.display());
    } else {
        std::fs::write(&org, STARTER_ORG)?;
        println!("created {:<15} {}", "org.toml", org.display());
    }
    println!(
        "\nnext: edit {} (grants, actions, budgets), then\n  roster worker init <name>\n  roster connection add anthropic   (a model credential — workers need one)\n  roster server validate\n  roster server start",
        org.display()
    );
    println!(
        "tip: `git init {}` gives your governance config a reviewable history",
        paths::config_root().display()
    );
    Ok(())
}
