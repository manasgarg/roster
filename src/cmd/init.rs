//! `roster init` — initialize a deployment: the XDG config/data/state roots
//! and a starter org.toml. Idempotent; never overwrites an existing file.

use super::BErr;
use crate::paths;

const STARTER_ORG: &str = r#"# Roster org config — ADMIN-ONLY. Applies to every worker (scope "org");
# per-worker overlays live in workers/<name>/worker.toml. Config loads live:
# check edits with `roster server validate`.

# Where the roster checkout lives — the box mounts pi + extensions from here
# (until they are baked into the box image).
[engine]
dir = "/path/to/roster"

# Egress grants shared by all workers, e.g.:
# [[grant]]
# name = "web-fetch"
# host = "*"
# methods = ["GET"]

# Actions workers may propose ([[action]]), the trust ladder ([[trust]]),
# and budgets ([budget] + [[budget.limit]]) — see docs/cli.md and org.toml
# in an existing deployment for worked examples.
"#;

pub fn run() -> Result<(), BErr> {
    let dirs = [
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
    ];
    for (label, dir) in dirs {
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
        "\nnext: edit {} (set [engine] dir), then\n  roster worker init <name>\n  roster server validate\n  roster server run",
        org.display()
    );
    println!(
        "tip: `git init {}` gives your governance config a reviewable history",
        paths::config_root().display()
    );
    Ok(())
}
