//! `impyard init` — initialize a deployment: the XDG config/data/state roots
//! and a starter org.toml. Idempotent; never overwrites an existing file.

use crate::paths;
use crate::util::BErr;

const STARTER_ORG: &str = r#"# Impyard org config — ADMIN-ONLY. Applies to every imp (scope "org");
# per-imp overlays live in imps/<name>/imp.toml. Config loads live:
# check edits with `impyard server validate`.

# pi + the box extensions are baked into the impyard-box image. Developers
# iterating on them can mount a checkout over the baked engine instead:
# [engine]
# dir = "/path/to/impyard"

# Egress grants shared by all imps, e.g.:
# [[grant]]
# name = "web-fetch"
# match = { host = "*", method = "GET" }
# verdict = "allow"

# Actions imps may propose ([[action]]), the trust ladder ([[trust]]),
# and budgets ([budget] + [[budget.limit]]) — see docs/configuration.md
# for the full reference and worked examples.
"#;

pub fn run() -> Result<(), BErr> {
    let dirs = [
        ("config", paths::config_root()),
        ("config/imps", paths::imps_dir()),
        ("data", paths::data_root()),
        ("data/vault", paths::vault_dir()),
        ("data/imps", paths::imps_data_dir()),
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
        "\nnext: edit {} (grants, actions, budgets), then\n  impyard imp init <name>\n  impyard server validate\n  impyard server start",
        org.display()
    );
    println!(
        "tip: `git init {}` gives your governance config a reviewable history",
        paths::config_root().display()
    );
    Ok(())
}
