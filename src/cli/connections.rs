//! `impyard connection add <service>` — the whole connection choreography in
//! one command: catalog lookup, login flow, vault store, and a scaffolded
//! `connections/<name>.toml` the admin owns from then on. And
//! `impyard connection ls` — the inventory.

use crate::util::BErr;
use serde_json::Value;

pub struct ConnectOptions {
    pub imps: Vec<String>,
    pub org: bool,
    pub alias: Option<String>,
    pub hosts: Vec<String>,
    pub header: Option<String>,
    pub env: Option<String>,
    pub methods: Vec<String>,
}

pub async fn connect(service: String, options: ConnectOptions) -> Result<(), BErr> {
    let ConnectOptions {
        imps,
        org,
        alias,
        hosts: host_overrides,
        header: header_override,
        env: env_override,
        methods: method_overrides,
    } = options;
    let registry = crate::credential::registry::registry_json();
    let name = alias.unwrap_or_else(|| service.clone());
    let path = crate::paths::connections_dir().join(format!("{name}.toml"));
    let existing: Option<toml::Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| toml::from_str(&text).ok());
    let registered = registry.get(&service).cloned();
    let catalog_meta = registered.as_ref().and_then(|p| p.get("connection"));
    let generic = catalog_meta.is_none() && (!host_overrides.is_empty() || existing.is_some());
    if registered.is_none() && !generic {
        print_catalog(&registry)?;
        return Err(format!(
            "\"{service}\" is not in the catalog — add --host <hostname> to create it"
        )
        .into());
    }
    if let Some(p) = registered
        .as_ref()
        .filter(|_| catalog_meta.is_none() && !generic)
    {
        let auth = p.get("auth").and_then(Value::as_str).unwrap_or("");
        if matches!(auth, "discord" | "smtp" | "slack") {
            return Err(format!(
                "\"{service}\" is host-side infrastructure, not an imp service connection — run: impyard credential add {service}"
            )
            .into());
        }
        return Err(format!(
            "\"{service}\" supplies authentication but is not an imp service connection — run: impyard credential add {service}"
        )
        .into());
    }

    let login_provider = registered
        .clone()
        .unwrap_or_else(|| serde_json::json!({ "auth": "api_key" }));
    let hosts = if host_overrides.is_empty() {
        if let Some(hosts) = catalog_meta.and_then(|meta| meta["hosts"].as_array()) {
            hosts
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        } else {
            toml_strings(existing.as_ref(), "hosts")
        }
    } else {
        host_overrides
    };
    let methods = if method_overrides.is_empty() {
        let existing_methods = toml_strings(existing.as_ref(), "methods");
        if existing_methods.is_empty() {
            vec!["GET".to_string()]
        } else {
            existing_methods
        }
    } else {
        method_overrides
            .into_iter()
            .map(|method| method.to_ascii_uppercase())
            .collect()
    };
    let env = env_override.unwrap_or_else(|| {
        catalog_meta
            .and_then(|meta| meta["env"].as_str())
            .map(str::to_string)
            .or_else(|| {
                existing
                    .as_ref()
                    .and_then(|v| v.get("env"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| default_env(&name))
    });
    let inline_header = match header_override {
        Some(header) => Some(parse_header(&header)?),
        None if generic => existing
            .as_ref()
            .and_then(|v| {
                Some((
                    v.get("inject_header")?.as_str()?.to_string(),
                    v.get("inject_value")?.as_str()?.to_string(),
                ))
            })
            .or_else(|| Some(("authorization".into(), "Bearer {key}".into()))),
        None => None,
    };

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
            Some(
                answer
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
            )
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
    let cred = crate::credential::connect::login(&service, &login_provider).await?;
    crate::credential::connect::store(&name, &cred)?;
    println!(
        "\n{} credential \"{name}\" in the vault",
        if rotating { "rotated" } else { "stored" }
    );

    // The connection file: scaffolded once, human-owned after. A rotation
    // never overwrites the admin's edits.
    let dir = crate::paths::connections_dir();
    if path.exists() {
        println!("kept    {} (edit it to change hosts/scope)", path.display());
    } else {
        std::fs::create_dir_all(&dir)?;
        let scope_line = match &scope_imps {
            None => "scope = \"org\"".to_string(),
            Some(list) => format!(
                "imps = [{}]",
                list.iter()
                    .map(|w| format!("\"{w}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        let hosts_line = hosts
            .iter()
            .map(|h| format!("\"{h}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let methods_line = methods
            .iter()
            .map(|method| format!("\"{method}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let inject_lines = inline_header
            .as_ref()
            .map(|(header, value)| {
                format!(
                    "inject_header = \"{}\"\ninject_value = \"{}\"\n",
                    toml_escape(header),
                    toml_escape(value)
                )
            })
            .unwrap_or_default();
        std::fs::write(
            &path,
            format!(
                "# Connection \"{name}\" — scaffolded by `impyard connection add`, yours to edit.\n\
                 # Compiles live into: an egress grant for these hosts/methods (evaluated\n\
                 # before hand-written grants), credential injection in transit, and the env\n\
                 # var below set in the box (to a sentinel; the secret never enters the box).\n\
                 provider = \"{service}\"\n\
                 {scope_line}\n\
                 hosts = [{hosts_line}]\n\
                 methods = [{methods_line}]\n\
                 env = \"{}\"\n\
                 {inject_lines}",
                toml_escape(&env)
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
            return Err(format!(
                "{} config error(s) — the connection is stored but config needs fixing",
                errors.len()
            )
            .into());
        }
    }
    Ok(())
}

fn default_env(name: &str) -> String {
    let mut stem: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    stem = stem.trim_matches('_').to_string();
    if stem.is_empty() {
        stem = "SERVICE".into();
    }
    if stem.starts_with(|c: char| c.is_ascii_digit()) {
        stem.insert_str(0, "SERVICE_");
    }
    format!("{stem}_TOKEN")
}

fn toml_strings(value: Option<&toml::Value>, key: &str) -> Vec<String> {
    value
        .and_then(|v| v.get(key))
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_header(input: &str) -> Result<(String, String), BErr> {
    let (name, value) = input
        .split_once(':')
        .ok_or("--header must look like: 'Authorization: Bearer {token}'")?;
    let name = name.trim();
    let value = value.trim();
    if name.is_empty() || value.is_empty() {
        return Err("--header needs both a header name and value template".into());
    }
    if !value.contains("{token}") && !value.contains("{key}") {
        return Err("--header value must contain {token}".into());
    }
    Ok((name.to_ascii_lowercase(), value.replace("{token}", "{key}")))
}

fn toml_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The services `connection add` can set up in one step, from the registry.
fn print_catalog(registry: &serde_json::Map<String, Value>) -> Result<(), BErr> {
    let mut names: Vec<&String> = registry
        .iter()
        .filter(|(_, p)| p.get("connection").is_some())
        .map(|(name, _)| name)
        .collect();
    names.sort();
    println!(
        "Services (impyard connection add <service> [--imp <name>].. [--org] [--name <name>]):"
    );
    let width = names.iter().map(|n| n.len()).max().unwrap_or(0);
    for n in names {
        let meta = &registry[n.as_str()]["connection"];
        let hosts: Vec<&str> = meta["hosts"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        println!(
            "  {n:width$}  {} → {}",
            hosts.join(", "),
            meta["env"].as_str().unwrap_or("?")
        );
    }
    println!("\nModel and channel credentials: impyard credential add <provider>");
    println!("Any other service: impyard connection add <name> --host <hostname>");
    Ok(())
}

pub fn catalog() -> Result<(), BErr> {
    print_catalog(&crate::credential::registry::registry_json())
}

/// `impyard connection ls` — every connection, its scope, and its state.
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
        println!("no connections — see the catalog: impyard connection catalog");
        return Ok(());
    }
    println!(
        "{:<14} {:<10} {:<18} {:<24} {:<14} STATE",
        "CONNECTION", "PROVIDER", "SCOPE", "HOSTS", "ENV"
    );
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
            if conn.enabled {
                "active"
            } else {
                "DISABLED (no secret)"
            }
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_defaults_are_safe_and_predictable() {
        assert_eq!(default_env("my-service"), "MY_SERVICE_TOKEN");
        assert_eq!(default_env("123"), "SERVICE_123_TOKEN");
        assert_eq!(
            parse_header("Authorization: Bearer {token}").unwrap(),
            ("authorization".into(), "Bearer {key}".into())
        );
        assert!(parse_header("Authorization: literal-secret").is_err());
    }
}
