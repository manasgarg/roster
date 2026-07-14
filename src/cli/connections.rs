//! `impyard server connect <service>` — the whole connection choreography in
//! one command: catalog lookup, login flow, vault store, and a scaffolded
//! `connections/<name>.toml` the admin owns from then on. And
//! `impyard server connections` — the inventory.

use crate::util::BErr;
use serde_json::Value;

pub async fn connect(
    service: Option<String>,
    imps: Vec<String>,
    org: bool,
    alias: Option<String>,
) -> Result<(), BErr> {
    let registry = crate::credential::registry::registry_json();
    let Some(service) = service else {
        return catalog(&registry);
    };
    let Some(p) = registry.get(&service).cloned() else {
        catalog(&registry)?;
        return Err(format!(
            "unknown service \"{service}\" — pick from the catalog above, or declare it in providers.toml (auth, inject, and a [<name>.connection] block) and re-run"
        )
        .into());
    };

    // Channel providers are host-consumed infrastructure, not connections:
    // the credential never enters a box. Vault step + the binding pointer.
    let auth = p.get("auth").and_then(Value::as_str).unwrap_or("");
    if matches!(auth, "discord" | "smtp" | "slack") {
        let cred = crate::credential::connect::login(&service, &p).await?;
        crate::credential::connect::store(&service, &cred)?;
        println!("\nconnected: \"{service}\" credential in the vault (channel infrastructure — never exposed to boxes)");
        if matches!(auth, "discord" | "slack") {
            println!("bind an imp to it: [channels] {auth} = \"{service}\" in imps/<name>/imp.toml");
        }
        return Ok(());
    }

    let Some(meta) = p.get("connection") else {
        return Err(format!(
            "\"{service}\" is a model provider wired via grants, not a service connection — add a `connection` block ({{ hosts, env }}) in providers.toml if it should become one"
        )
        .into());
    };
    let name = alias.unwrap_or_else(|| service.clone());

    // Scope: flags win; otherwise ask. Per-imp is the default posture —
    // a connection is a capability granted to an identity, not to the fleet.
    let known = match crate::config::snapshot() {
        Ok(c) => c.imps.clone(),
        Err(e) => return Err(format!("config must load before connecting a service:\n{e}").into()),
    };
    let scope_imps: Option<Vec<String>> = if org {
        None
    } else if !imps.is_empty() {
        Some(imps)
    } else {
        let answer = crate::credential::connect::ask(&format!(
            "for which imp(s)? ({}, comma-separated, or \"org\" for org-wide): ",
            known.join(", ")
        ))?;
        if answer.trim() == "org" {
            None
        } else {
            Some(answer.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        }
    };
    if let Some(list) = &scope_imps {
        if list.is_empty() {
            return Err("no imps named — nothing to grant".into());
        }
        for w in list {
            if !known.contains(w) {
                return Err(format!("no such imp \"{w}\" (have: {})", known.join(", ")).into());
            }
        }
    }

    // The secret. Re-connecting an existing connection rotates it in place.
    let rotating = crate::credential::vault::get_credential(&name).is_some();
    let cred = crate::credential::connect::login(&service, &p).await?;
    crate::credential::connect::store(&name, &cred)?;
    println!(
        "\n{} credential \"{name}\" in the vault",
        if rotating { "rotated" } else { "stored" }
    );

    // The connection file: scaffolded once, human-owned after. A rotation
    // never overwrites the admin's edits.
    let dir = crate::paths::connections_dir();
    let path = dir.join(format!("{name}.toml"));
    if path.exists() {
        println!("kept    {} (edit it to change hosts/scope)", path.display());
    } else {
        std::fs::create_dir_all(&dir)?;
        let hosts: Vec<String> = meta["hosts"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        let env = meta["env"].as_str().unwrap_or("SERVICE_TOKEN");
        let scope_line = match &scope_imps {
            None => "scope = \"org\"".to_string(),
            Some(list) => format!(
                "imps = [{}]",
                list.iter().map(|w| format!("\"{w}\"")).collect::<Vec<_>>().join(", ")
            ),
        };
        let hosts_line = hosts.iter().map(|h| format!("\"{h}\"")).collect::<Vec<_>>().join(", ");
        std::fs::write(
            &path,
            format!(
                "# Connection \"{name}\" — scaffolded by `impyard server connect`, yours to edit.\n\
                 # Compiles live into: an egress grant for these hosts/methods (evaluated\n\
                 # before hand-written grants), credential injection in transit, and the env\n\
                 # var below set in the box (to a sentinel; the secret never enters the box).\n\
                 provider = \"{service}\"\n\
                 {scope_line}\n\
                 hosts = [{hosts_line}]\n\
                 methods = [\"GET\"]\n\
                 env = \"{env}\"\n"
            ),
        )?;
        println!("created {}", path.display());
    }

    // Show the compiled result (or every error, if the admin's config is off).
    match crate::config::load() {
        Ok(c) => {
            for w in &c.warnings {
                println!("warning: {w}");
            }
            if let Some(conn) = c.connections.iter().find(|c| c.name == name) {
                let scope = match &conn.imps {
                    None => "org-wide".to_string(),
                    Some(l) => l.join(", "),
                };
                println!(
                    "active: {} → {} [{}] as {} for {}",
                    conn.name,
                    conn.hosts.join(", "),
                    conn.methods.join(", "),
                    conn.env,
                    scope
                );
            }
        }
        Err(errors) => {
            for e in &errors {
                eprintln!("config: {e}");
            }
            return Err(format!("{} config error(s) — the connection is stored but config needs fixing", errors.len()).into());
        }
    }
    Ok(())
}

/// The services `connect` can set up in one step, from the registry.
fn catalog(registry: &serde_json::Map<String, Value>) -> Result<(), BErr> {
    let mut names: Vec<&String> = registry
        .iter()
        .filter(|(_, p)| p.get("connection").is_some())
        .map(|(n, _)| n)
        .collect();
    names.sort();
    println!("Services (impyard server connect <service> [--imp <name>].. [--org] [--as <name>]):");
    let width = names.iter().map(|n| n.len()).max().unwrap_or(0);
    for n in names {
        let meta = &registry[n.as_str()]["connection"];
        let hosts: Vec<&str> = meta["hosts"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        println!("  {n:width$}  {} → {}", hosts.join(", "), meta["env"].as_str().unwrap_or("?"));
    }
    println!("\nChannels (discord, slack, smtp) stay infrastructure: impyard server vault connect");
    println!("<provider>, then bind in imp.toml. Custom services: add [<name>] with auth/inject");
    println!("and a connection block to providers.toml — connect picks it up.");
    Ok(())
}

/// `impyard server connections` — every connection, its scope, and its state.
pub fn ls(json: bool) -> Result<(), BErr> {
    let c = crate::config::snapshot().map_err(|e| format!("config invalid:\n{e}"))?;
    if json {
        let out: Vec<Value> = c
            .connections
            .iter()
            .map(|c| {
                serde_json::json!({
                    "name": c.name, "provider": c.provider,
                    "imps": c.imps, "hosts": c.hosts,
                    "methods": c.methods, "env": c.env, "enabled": c.enabled,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    if c.connections.is_empty() {
        println!("no connections — see the catalog: impyard server connect");
        return Ok(());
    }
    println!("{:<14} {:<10} {:<18} {:<24} {:<14} STATE", "CONNECTION", "PROVIDER", "SCOPE", "HOSTS", "ENV");
    for conn in &c.connections {
        let scope = match &conn.imps {
            None => "org".to_string(),
            Some(l) => l.join(","),
        };
        println!(
            "{:<14} {:<10} {:<18} {:<24} {:<14} {}",
            conn.name,
            conn.provider,
            scope,
            conn.hosts.join(","),
            conn.env,
            if conn.enabled { "active" } else { "DISABLED (no secret)" }
        );
    }
    Ok(())
}
