//! `roster connection …` — the one noun (docs/connections.md). A
//! connection is roster's relationship with an external service: an identity
//! (a secret in the vault) plus one or more uses — capability (a box acts on
//! the service), channel (the worker speaks through it), model (grants inject
//! it). Uses are derived from the binding surfaces, never stored.
//!
//! `add` runs the whole choreography for any provider: registry lookup (or
//! the unknown-service interview), login flow, vault store, per-use
//! follow-through. `ls` merges every surface into one inventory. `rm` deletes
//! the secret and reports every surviving reference.

use crate::util::BErr;
use serde_json::Value;
use std::path::PathBuf;

#[derive(Default)]
pub struct ConnectOptions {
    pub workers: Vec<String>,
    pub org: bool,
    pub alias: Option<String>,
    pub hosts: Vec<String>,
    pub header: Option<String>,
    pub env: Option<String>,
    pub methods: Vec<String>,
    /// Which uses to set up (`--use`); empty = all the provider supports
    /// (asked interactively when there are several).
    pub uses: Vec<String>,
    /// Auth method when the provider offers several (`--auth`).
    pub auth: Option<String>,
    /// Interview for an unknown service first (`--declare`).
    pub declare: bool,
    /// Test the stored credential against the live service (`--verify`).
    pub verify: bool,
}

pub async fn connect(service: String, options: ConnectOptions) -> Result<(), BErr> {
    let ConnectOptions {
        workers,
        org,
        alias,
        hosts: mut host_overrides,
        header: mut header_override,
        env: mut env_override,
        methods: mut method_overrides,
        uses: use_flags,
        auth: auth_override,
        declare,
        verify,
    } = options;

    let mut registry = crate::credential::registry::registry_json();

    // The retired half of the old slack split: hidden alias, loud pointer.
    if service == "slack-api" {
        return Err(
            "\"slack-api\" merged into \"slack\" — run: roster connection add slack --use capability"
                .into(),
        );
    }

    if declare {
        if registry.contains_key(&service) {
            return Err(format!(
                "\"{service}\" is already in the registry — edit {} to change it",
                crate::paths::providers_file().display()
            )
            .into());
        }
        match interview_unknown(&service)? {
            Declared::Inline {
                hosts,
                header,
                env,
                methods,
            } => {
                if host_overrides.is_empty() {
                    host_overrides = hosts;
                }
                if header_override.is_none() {
                    header_override = header;
                }
                if env_override.is_none() {
                    env_override = env;
                }
                if method_overrides.is_empty() {
                    method_overrides = methods;
                }
            }
            // The interview wrote a providers.toml entry — read it back.
            Declared::Registered => registry = crate::credential::registry::registry_json(),
        }
    }

    let name = alias.unwrap_or_else(|| service.clone());
    // The name and service become bare TOML keys ([name], [service]) and file
    // names; a space/quote/bracket would corrupt providers.toml (whose parse
    // failure silently drops the WHOLE overlay) or write a bad path. Reject up
    // front rather than escaping a header that references elsewhere wouldn't match.
    for (label, v) in [("connection name", &name), ("service", &service)] {
        let bare = !v.is_empty()
            && v.as_bytes()[0] != b'-'
            && v.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
        if !bare {
            return Err(format!(
                "{label} must be lowercase letters, digits, - or _ (a bare identifier): \"{v}\""
            )
            .into());
        }
    }
    let path = crate::paths::connections_dir().join(format!("{name}.toml"));
    let existing: Option<toml::Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| toml::from_str(&text).ok());
    let registered = registry.get(&service).cloned();
    let generic = registered.is_none() && (!host_overrides.is_empty() || existing.is_some());
    if registered.is_none() && !generic {
        print_catalog(&registry)?;
        return Err(format!(
            "\"{service}\" is not in the catalog — add --host <hostname> for a token API, or --declare for OAuth"
        )
        .into());
    }

    // Which uses? Flags win; one supported use goes without asking; several ask.
    let supported: Vec<String> = registered
        .as_ref()
        .map(crate::credential::registry::provider_uses)
        .unwrap_or_else(|| vec!["capability".into()]);
    let uses: Vec<String> = if use_flags.is_empty() {
        if supported.len() <= 1 {
            supported.clone()
        } else {
            ask_uses(&service, &supported)?
        }
    } else {
        let mut chosen = Vec::new();
        for u in use_flags {
            if !supported.contains(&u) {
                return Err(format!(
                    "\"{service}\" does not support the \"{u}\" use (supports: {})",
                    supported.join(", ")
                )
                .into());
            }
            if !chosen.contains(&u) {
                chosen.push(u);
            }
        }
        chosen
    };
    let capability = uses.iter().any(|u| u == "capability");
    let channel = uses.iter().any(|u| u == "channel");
    let model = uses.iter().any(|u| u == "model");

    // --auth: pick among the entry's offered methods. "auth" is a string or
    // a list whose first entry is the default.
    let offered: Vec<String> = match registered.as_ref().and_then(|p| p.get("auth")) {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(l)) => l
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => vec!["api_key".to_string()],
    };
    let chosen_auth = match &auth_override {
        Some(want) if offered.contains(want) => want.clone(),
        Some(_) => {
            return Err(format!("\"{service}\" offers {} auth", offered.join(" or ")).into());
        }
        None => offered
            .first()
            .cloned()
            .unwrap_or_else(|| "api_key".to_string()),
    };

    // Capability fields: catalog defaults ← existing file ← flags.
    let catalog_meta = registered
        .as_ref()
        .and_then(|p| p.get("connection"))
        .cloned();
    let hosts = if host_overrides.is_empty() {
        if let Some(hosts) = catalog_meta
            .as_ref()
            .and_then(|meta| meta["hosts"].as_array())
        {
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
            // Full access by default: connecting a service means the worker can
            // use it. `--method` (or editing the file) narrows it afterwards.
            vec!["*".to_string()]
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
            .as_ref()
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

    // Scope: flags win; otherwise ask — but only when a capability file will
    // be scaffolded. Per-worker is the default posture: a connection is a
    // capability granted to an identity, not to the fleet.
    let known = match crate::config::snapshot() {
        Ok(c) => c.workers.clone(),
        Err(e) => return Err(format!("config must load before connecting a service:\n{e}").into()),
    };
    let scope_workers: Option<Vec<String>> = if !capability || path.exists() || org {
        None
    } else if !workers.is_empty() {
        Some(workers.clone())
    } else {
        let answer = crate::credential::connect::ask(&format!(
            "for which worker(s)? ({}, comma-separated, or \"org\" for org-wide): ",
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
    if capability && !path.exists() {
        if let Some(list) = &scope_workers {
            if list.is_empty() {
                return Err("no workers named — nothing to grant".into());
            }
            for w in list {
                if !known.contains(w) {
                    return Err(
                        format!("no such worker \"{w}\" (have: {})", known.join(", ")).into(),
                    );
                }
            }
        }
    }

    // The secret. Re-connecting rotates it in place; a credential that a
    // channel listener already consumes keeps its channel-only fields even
    // when this add is for another use (rotation must not break the bot).
    let channel_bound = !channel_binding_refs(&name).is_empty();
    let rotating = crate::credential::vault::get_credential(&name).is_some();
    let mut login_provider = registered
        .clone()
        .unwrap_or_else(|| serde_json::json!({ "auth": "api_key" }));
    login_provider["auth"] = serde_json::json!(chosen_auth);
    let cred =
        crate::credential::connect::login(&service, &login_provider, channel || channel_bound)
            .await?;
    crate::credential::connect::store(&name, &cred)?;
    println!(
        "\n{} credential \"{name}\" in the vault",
        if rotating { "rotated" } else { "stored" }
    );

    // Per-use follow-through.
    if capability {
        scaffold_connection(
            &path,
            &name,
            &service,
            &scope_workers,
            &hosts,
            &methods,
            &env,
            &inline_header,
        )?;
    }
    if channel {
        bind_channel(&service, &name, &workers)?;
    }
    if model {
        ensure_model_connection(&name, &service, &workers)?;
    }

    // The compiled result (or every error, if the admin's config is off).
    match crate::config::load() {
        Ok(c) => {
            for w in &c.warnings {
                println!("warning: {w}");
            }
            if capability {
                if let Some(conn) = c.connections.iter().find(|c| c.name == name) {
                    let scope = match &conn.workers {
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
            if channel {
                for (worker, platform, credential) in &c.listeners {
                    if credential == &name {
                        println!("listening: {platform} for {worker} with \"{name}\"");
                    }
                }
            }
            if model {
                report_model(&c, &name, registered.as_ref());
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
    if verify {
        verify_connection(&name, &service).await?;
    }
    Ok(())
}

/// `--verify`: one authenticated call against the live service, per the
/// provider's registry recipe, so a mistyped token fails here — not later,
/// silently, inside a worker's run. No recipe (custom services) = skipped.
async fn verify_connection(name: &str, service: &str) -> Result<(), BErr> {
    let registry = crate::credential::registry::registry_json();
    let Some(recipe) = registry.get(service).and_then(|p| p.get("verify")).cloned() else {
        println!("verify: no recipe for \"{service}\" — skipped");
        return Ok(());
    };
    let Some(cred) = crate::credential::vault::get_credential(name) else {
        return Err(format!("verify: credential \"{name}\" is not in the vault").into());
    };
    let method = recipe
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("GET");
    let url = recipe
        .get("url")
        .and_then(Value::as_str)
        .ok_or("verify recipe has no url")?;
    let mut req = reqwest::Client::new()
        .request(method.parse::<reqwest::Method>()?, url)
        .timeout(std::time::Duration::from_secs(10));
    if let Some(headers) = recipe.get("headers").and_then(Value::as_object) {
        for (k, v) in headers {
            if let Some(v) = v.as_str() {
                req = req.header(k.as_str(), v);
            }
        }
    }
    for (k, v) in crate::credential::vault::render_injection(&cred, service) {
        req = req.header(k.as_str(), v);
    }
    if let Some(body) = recipe.get("body").and_then(Value::as_str) {
        req = req
            .header("content-type", "application/json")
            .body(body.to_string());
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("verify: could not reach {url}: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    // Some APIs (slack) answer 200 with an error body — the recipe may pin a
    // marker the success body must contain.
    let body_ok = recipe
        .get("body_contains")
        .and_then(Value::as_str)
        .map(|marker| text.contains(marker))
        .unwrap_or(true);
    if status.is_success() && body_ok {
        println!("verified: {method} {url} → {status}");
        Ok(())
    } else if status.as_u16() == 401 || status.as_u16() == 403 || (status.is_success() && !body_ok)
    {
        Err(format!(
            "verify FAILED: {method} {url} → {status} — the service rejected the credential; \
             paste a fresh one: roster connection add {service}"
        )
        .into())
    } else {
        println!(
            "verify inconclusive: {method} {url} → {status} (the credential may still be fine)"
        );
        Ok(())
    }
}

/// The connection file: scaffolded once, human-owned after. A rotation never
/// overwrites the admin's edits.
#[allow(clippy::too_many_arguments)]
fn scaffold_connection(
    path: &std::path::Path,
    name: &str,
    service: &str,
    scope_workers: &Option<Vec<String>>,
    hosts: &[String],
    methods: &[String],
    env: &str,
    inline_header: &Option<(String, String)>,
) -> Result<(), BErr> {
    if path.exists() {
        println!("kept    {} (edit it to change hosts/scope)", path.display());
        return Ok(());
    }
    std::fs::create_dir_all(crate::paths::connections_dir())?;
    let scope_line = match scope_workers {
        None => "scope = \"org\"".to_string(),
        Some(list) => format!(
            "workers = [{}]",
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
        path,
        format!(
            "# Connection \"{name}\" — scaffolded by `roster connection add`, yours to edit.\n\
             # Compiles live into: an egress grant for these hosts/methods (evaluated\n\
             # before hand-written grants), credential injection in transit, and the env\n\
             # var below set in the box (to a sentinel; the secret never enters the box).\n\
             provider = \"{service}\"\n\
             {scope_line}\n\
             hosts = [{hosts_line}]\n\
             methods = [{methods_line}]\n\
             env = \"{}\"\n\
             {inject_lines}",
            toml_escape(env)
        ),
    )?;
    println!("created {}", path.display());
    Ok(())
}

/// Channel follow-through: offer the `[channels]` binding and write it
/// (offer-and-write: a hand-carried snippet is exactly the two-step the
/// one-noun surface exists to kill). One credential serves one listener.
fn bind_channel(platform: &str, credential: &str, worker_flags: &[String]) -> Result<(), BErr> {
    if platform == "smtp" {
        println!("smtp is consumed host-side by the email executor — no worker binding needed");
        return Ok(());
    }
    let snippet = format!("  [channels]\n  {platform} = \"{credential}\"");
    let known = crate::worker::names();
    if known.is_empty() {
        println!(
            "no workers yet — after `roster worker init <name>`, bind it in the worker's spec:\n{snippet}"
        );
        return Ok(());
    }
    let worker = if worker_flags.len() == 1 {
        worker_flags[0].clone()
    } else if worker_flags.len() > 1 {
        println!(
            "a channel binding takes one worker (one bot cannot serve two listeners) — \
             bind by hand, with --name for a second credential:\n{snippet}"
        );
        return Ok(());
    } else {
        let answer = crate::credential::connect::ask(&format!(
            "bind the {platform} listener to which worker? ({}, or Enter to skip): ",
            known.join(", ")
        ))?;
        let answer = answer.trim().to_string();
        if answer.is_empty() {
            println!("skipped — bind it later in the worker's spec:\n{snippet}");
            return Ok(());
        }
        answer
    };
    if !known.contains(&worker) {
        return Err(format!("no such worker \"{worker}\" (have: {})", known.join(", ")).into());
    }
    let spec = crate::paths::workers_dir()
        .join(&worker)
        .join("worker.toml");
    let text = std::fs::read_to_string(&spec)?;
    match upsert_channel_binding(&text, platform, credential) {
        Ok(Some(new_text)) => {
            std::fs::write(&spec, new_text)?;
            println!(
                "bound   {platform} = \"{credential}\" in {}",
                spec.display()
            );
        }
        Ok(None) => println!("kept    \"{worker}\" already binds {platform} = \"{credential}\""),
        Err(e) => return Err(format!("worker \"{worker}\": {e}").into()),
    }
    Ok(())
}

/// Set `platform = "credential"` in a worker.toml's `[channels]` table,
/// textually, preserving the admin's comments and layout. `Ok(None)` = the
/// exact binding is already there; `Err` = a conflicting binding (edits to a
/// human-owned file are the human's).
fn upsert_channel_binding(
    text: &str,
    platform: &str,
    credential: &str,
) -> Result<Option<String>, String> {
    let parsed: toml::Value =
        toml::from_str(text).map_err(|e| format!("worker.toml is invalid: {e}"))?;
    if let Some(current) = parsed
        .get("channels")
        .and_then(|c| c.get(platform))
        .and_then(|v| v.as_str())
    {
        if current == credential {
            return Ok(None);
        }
        return Err(format!(
            "already binds {platform} = \"{current}\" — edit worker.toml to change it"
        ));
    }
    let line = format!("{platform} = \"{credential}\"");
    let mut out = String::new();
    let mut inserted = false;
    for l in text.lines() {
        out.push_str(l);
        out.push('\n');
        if inserted {
            continue;
        }
        let t = l.trim();
        let is_header = t == "[channels]"
            || t.strip_prefix("[channels]")
                .is_some_and(|rest| rest.trim_start().starts_with('#'));
        if is_header {
            out.push_str(&line);
            out.push('\n');
            inserted = true;
        }
    }
    if !inserted {
        if parsed.get("channels").is_some() {
            // e.g. an inline `channels = { … }` table — not ours to rewrite.
            return Err(format!(
                "has a [channels] table this tool cannot edit — add {line} by hand"
            ));
        }
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push('\n');
        }
        out.push_str("[channels]\n");
        out.push_str(&line);
        out.push('\n');
    }
    Ok(Some(out))
}

/// A model connection IS a grant by default: scaffold the connection file
/// that compiles into one (allow + inject on the provider's model hosts,
/// GET/POST, no env exposure). Admin-owned after creation — edit or delete
/// it to change access. Returns whether a file was written.
pub fn ensure_model_connection(
    name: &str,
    service: &str,
    workers: &[String],
) -> Result<bool, BErr> {
    let path = crate::paths::connections_dir().join(format!("{name}.toml"));
    if path.exists() {
        return Ok(false);
    }
    std::fs::create_dir_all(crate::paths::connections_dir())?;
    let scope_line = if workers.is_empty() {
        "scope = \"org\"".to_string()
    } else {
        format!(
            "workers = [{}]",
            workers
                .iter()
                .map(|w| format!("\"{w}\""))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    std::fs::write(
        &path,
        format!(
            "# Model connection — workers' boxes may call {service}'s model API; the\n\
             # gateway injects the credential in transit. Hosts derive from the provider\n\
             # registry; all methods are allowed — narrow with hosts = [..] / methods = [..].\n\
             # ADMIN-OWNED after creation: edit or delete this file to change access.\n\
             provider = \"{service}\"\n\
             {scope_line}\n"
        ),
    )?;
    println!(
        "created {}   (the model grant — edit or delete it to change access)",
        path.display()
    );
    Ok(true)
}

/// Grant-by-default healing: a vault model credential that no rule injects
/// gets its connection file scaffolded. A hand-written grant elsewhere wins —
/// nothing is scaffolded over it.
pub fn ensure_model_grant(name: &str) -> Result<(), BErr> {
    if let Ok(c) = crate::config::load() {
        let injected = c
            .policy
            .rules
            .iter()
            .any(|r| r.inject.as_ref().is_some_and(|i| i.credential == name));
        if !injected {
            ensure_model_connection(name, name, &[])?;
        }
    }
    Ok(())
}

/// Model follow-through: report the grants that inject this credential —
/// the connection's own by-default grant, or the admin's hand-written one —
/// with a starter block as the fallback when neither exists.
fn report_model(c: &crate::config::Loaded, name: &str, provider: Option<&Value>) {
    let grants: Vec<String> = c
        .policy
        .rules
        .iter()
        .filter(|r| r.inject.as_ref().is_some_and(|i| i.credential == name))
        .map(|r| format!("\"{}\" ({})", r.name, r.scope))
        .collect();
    if grants.is_empty() {
        let hosts = provider
            .and_then(|p| p.get("model_hosts"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(|h| format!("\"{h}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_else(|| "\"<the provider's API host>\"".into());
        println!(
            "\nno grant injects \"{name}\" yet — grants are yours to write; a starter for org.toml:\n\n\
             [[grant]]\n\
             name    = \"model-api\"\n\
             match   = {{ host = [{hosts}], port = 443 }}\n\
             verdict = \"allow\"\n\
             inject  = {{ credential = \"{name}\" }}"
        );
    } else {
        println!("active: injected by grant {}", grants.join(", "));
    }
}

// ── the guided session and the unknown-service interview ────────────────────

/// Bare `roster connection add`: open on the catalog, pick or interview.
pub async fn guided() -> Result<(), BErr> {
    let registry = crate::credential::registry::registry_json();
    print_catalog(&registry)?;
    let service = crate::credential::connect::ask(
        "\nservice to connect (a name above, or a new one; Enter to quit): ",
    )?
    .trim()
    .to_string();
    if service.is_empty() {
        return Ok(());
    }
    let mut options = ConnectOptions::default();
    let known = registry
        .get(&service)
        .is_some_and(|p| !crate::credential::registry::is_hidden(p))
        || service == "slack-api";
    if !known {
        match interview_unknown(&service)? {
            Declared::Inline {
                hosts,
                header,
                env,
                methods,
            } => {
                options.hosts = hosts;
                options.header = header;
                options.env = env;
                options.methods = methods;
            }
            Declared::Registered => {}
        }
    }
    connect(service, options).await
}

/// What the interview produced: an inline (key-shaped) definition that lives
/// in the connection file, or a providers.toml entry already written
/// (kind-shaped OAuth knowledge shared by every connection to the service).
enum Declared {
    Inline {
        hosts: Vec<String>,
        header: Option<String>,
        env: Option<String>,
        methods: Vec<String>,
    },
    Registered,
}

fn interview_unknown(service: &str) -> Result<Declared, BErr> {
    println!("\"{service}\" is not in the catalog — a few questions.");
    let kind = ask_default(
        "auth: [1] paste an API key/token  [2] OAuth (PKCE; needs your own app registration)",
        "1",
    )?;
    if kind == "2" {
        declare_oauth(service)?;
        return Ok(Declared::Registered);
    }
    let host = crate::credential::connect::ask(&format!("API host (e.g. api.{service}.com): "))?
        .trim()
        .to_string();
    if host.is_empty() {
        return Err("an API host is required".into());
    }
    let header = ask_default("header template", "Authorization: Bearer {token}")?;
    let env = ask_default("env var the box sees", &default_env(service))?;
    let methods: Vec<String> = ask_default("allowed methods (comma-separated, * = all)", "*")?
        .split(',')
        .map(|s| s.trim().to_ascii_uppercase())
        .filter(|s| !s.is_empty())
        .collect();
    Ok(Declared::Inline {
        hosts: vec![host],
        header: Some(header),
        env: Some(env),
        methods,
    })
}

/// Interview for an OAuth provider and append the entry to providers.toml —
/// kind-knowledge lands in the registry, where every connection to the
/// service shares it and the gateway finds the refresh endpoint.
fn declare_oauth(name: &str) -> Result<(), BErr> {
    let file = crate::paths::providers_file();
    println!(
        "declaring \"{name}\" (PKCE — register an app with the service first; roster ships no client ids):"
    );
    let ask = crate::credential::connect::ask;
    let must = |q: &str| -> Result<String, BErr> {
        let v = ask(q)?.trim().to_string();
        if v.is_empty() {
            return Err(format!("{} is required", q.trim_end_matches(": ")).into());
        }
        Ok(v)
    };
    let authorize_url = must("authorize URL: ")?;
    let token_url = must("token URL: ")?;
    let client_id = must("client id (from your app registration): ")?;
    let redirect_uri = ask_default("redirect URI", "http://localhost:1455/callback")?;
    let scope = ask("scopes (space-separated; Enter for none): ")?
        .trim()
        .to_string();
    let token_encoding = ask_default("token request encoding (json/form)", "json")?;
    if token_encoding != "json" && token_encoding != "form" {
        return Err("token encoding must be \"json\" or \"form\"".into());
    }
    let inject_header = ask_default("inject header", "authorization")?;
    let inject_value = ask_default("inject template", "Bearer {access}")?;
    let hosts_answer =
        ask("API hosts the box may call, comma-separated (Enter for none — model-style): ")?;
    let hosts: Vec<String> = hosts_answer
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let connection_line = if hosts.is_empty() {
        String::new()
    } else {
        let env = ask_default("env var the box sees", &default_env(name))?;
        format!(
            "connection = {{ hosts = [{}], env = \"{}\" }}\n",
            hosts
                .iter()
                .map(|h| format!("\"{}\"", toml_escape(h)))
                .collect::<Vec<_>>()
                .join(", "),
            toml_escape(&env)
        )
    };

    let entry = format!(
        "\n# \"{name}\" — declared by `roster connection add`, yours to edit.\n\
         [{name}]\n\
         auth = \"oauth\"\n\
         client_id = \"{}\"\n\
         token_url = \"{}\"\n\
         token_encoding = \"{token_encoding}\"\n\
         inject = [{{ header = \"{}\", value = \"{}\" }}]\n\
         {connection_line}\
         \n\
         [{name}.login]\n\
         flow = \"pkce\"\n\
         authorize_url = \"{}\"\n\
         redirect_uri = \"{}\"\n\
         scope = \"{}\"\n",
        toml_escape(&client_id),
        toml_escape(&token_url),
        toml_escape(&inject_header),
        toml_escape(&inject_value),
        toml_escape(&authorize_url),
        toml_escape(&redirect_uri),
        toml_escape(&scope),
    );
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = std::fs::read_to_string(&file).unwrap_or_default();
    text.push_str(&entry);
    std::fs::write(&file, text)?;
    println!("declared \"{name}\" in {}", file.display());
    Ok(())
}

// ── inventory ────────────────────────────────────────────────────────────────

/// `roster connection ls` — every connection, its use(s), and its state.
/// Uses are derived from what references the secret: a connection file
/// (capability), a `[channels]` binding (channel), a grant's inject (model).
/// A secret nothing references is `unbound`, not an error.
pub fn ls(json: bool) -> Result<(), BErr> {
    let c = crate::config::snapshot().map_err(|e| format!("config invalid:\n{e}"))?;
    let secret = |name: &str| crate::credential::vault::get_credential(name).is_some();
    let mut rows: Vec<Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = Default::default();

    for conn in &c.connections {
        seen.insert(conn.name.clone());
        let scope = match &conn.workers {
            None => "org".to_string(),
            Some(l) => l.join(","),
        };
        // A connection with no env exposure is a model connection: boxes
        // authenticate via sentinel logins, injection happens in transit.
        let is_model = conn.env.is_empty();
        rows.push(serde_json::json!({
            "name": conn.name,
            "use": if is_model { "model" } else { "capability" },
            "provider": conn.provider,
            "workers": conn.workers, "hosts": conn.hosts, "methods": conn.methods,
            "env": conn.env,
            "state": if conn.enabled { "active" } else { "DISABLED (no secret)" },
            "detail": if is_model {
                format!("{scope}: {} [{}], injected in transit",
                    conn.hosts.join(","), conn.methods.join(","))
            } else {
                format!("{scope}: {} [{}] → {}",
                    conn.hosts.join(","), conn.methods.join(","), conn.env)
            },
        }));
    }
    // The built-in store: every worker's auto-provisioned rw host-dir grant.
    for w in &c.workers {
        rows.push(serde_json::json!({
            "name": format!("store:{w}"), "use": "mount", "kind": "host-dir",
            "workers": [w], "path": crate::paths::worker_store_dir(w),
            "state": "active",
            "detail": format!("{w}: built-in rw store at $HOME/store (snapshotted, restorable)"),
        }));
    }
    for m in &c.host_mounts {
        seen.insert(m.name.clone());
        let scope = match &m.workers {
            None => "org".to_string(),
            Some(l) => l.join(","),
        };
        let (kind, access) = match &m.kind {
            crate::config::HostMountKind::Dir { rw } => {
                ("host-dir", if *rw { "rw".to_string() } else { "ro".to_string() })
            }
            crate::config::HostMountKind::Repo { gated, branch } => (
                "host-repo",
                if *gated {
                    format!("gated → {branch}")
                } else {
                    "ro".to_string()
                },
            ),
        };
        rows.push(serde_json::json!({
            "name": m.name, "use": "mount", "kind": kind,
            "workers": m.workers, "path": m.path,
            "state": "active",
            "detail": format!("{scope}: {} ({access}) at $HOME/mnt/{}", m.path.display(), m.name),
        }));
    }
    for (worker, platform, credential) in &c.listeners {
        seen.insert(credential.clone());
        rows.push(serde_json::json!({
            "name": credential, "use": "channel", "platform": platform, "worker": worker,
            "state": if secret(credential) { "active" } else { "DISABLED (no secret)" },
            "detail": format!("{platform} listener for {worker}"),
        }));
    }
    let mut model: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for r in &c.policy.rules {
        if r.name.starts_with("connection:") {
            continue;
        }
        if let Some(inject) = &r.inject {
            model
                .entry(inject.credential.clone())
                .or_default()
                .push(format!("\"{}\" ({})", r.name, r.scope));
        }
    }
    for (credential, grants) in model {
        seen.insert(credential.clone());
        rows.push(serde_json::json!({
            "name": credential, "use": "model", "grants": grants,
            "state": if secret(&credential) { "active" } else { "DISABLED (no secret)" },
            "detail": format!("injected by grant {}", grants.join(", ")),
        }));
    }
    for (name, kind) in vault_entries() {
        if seen.contains(&name) {
            continue;
        }
        // The email executor consumes the vault entry named "smtp" directly
        // (exec_email) — a host-side use no config surface records.
        if name == "smtp" {
            rows.push(serde_json::json!({
                "name": name, "use": "channel", "type": kind, "state": "active",
                "detail": "smtp — consumed host-side by the email executor",
            }));
            continue;
        }
        rows.push(serde_json::json!({
            "name": name, "use": Value::Null, "type": kind, "state": "unbound",
            "detail": format!("{kind} secret — nothing references it"),
        }));
    }
    rows.sort_by(|a, b| {
        (a["name"].as_str(), a["use"].as_str()).cmp(&(b["name"].as_str(), b["use"].as_str()))
    });

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no connections — see the catalog: roster connection catalog");
        return Ok(());
    }
    println!("{:<16} {:<11} {:<44} STATE", "CONNECTION", "USE", "DETAIL");
    for row in &rows {
        println!(
            "{:<16} {:<11} {:<44} {}",
            row["name"].as_str().unwrap_or("?"),
            row["use"].as_str().unwrap_or("—"),
            row["detail"].as_str().unwrap_or(""),
            row["state"].as_str().unwrap_or("?"),
        );
    }
    Ok(())
}

/// Vault names and types — never values.
fn vault_entries() -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(crate::credential::vault::vault_dir()) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let kind = std::fs::read_to_string(&path)
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(String::from))
                .unwrap_or_else(|| "?".into());
            out.push((name, kind));
        }
    }
    out.sort();
    out
}

// ── removal ──────────────────────────────────────────────────────────────────

/// `roster connection rm <name>` — delete the secret, then offer to delete the
/// connection file `add` scaffolded. References inside org.toml and
/// worker.toml stay report-only: those are the admin's files, rm never edits
/// them.
pub fn rm(name: &str) -> Result<(), BErr> {
    let secret = crate::credential::vault::vault_dir().join(format!("{name}.json"));
    if !secret.exists() && references(name).is_empty() {
        return Err(format!(
            "no connection \"{name}\" — nothing in the vault, nothing referencing it"
        )
        .into());
    }
    if secret.exists() {
        let answer = crate::credential::connect::ask(&format!(
            "delete the \"{name}\" secret from the vault? [y/N] "
        ))?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("kept");
            return Ok(());
        }
        std::fs::remove_file(&secret)?;
        println!("deleted vault secret \"{name}\"");
    } else {
        println!("no \"{name}\" secret in the vault (already gone)");
    }
    let conn = crate::paths::connections_dir().join(format!("{name}.toml"));
    if conn.exists() {
        let answer = crate::credential::connect::ask(&format!(
            "also delete connection file {} (drops the grant and exposure)? [y/N] ",
            conn.display()
        ))?;
        if matches!(answer.trim(), "y" | "Y" | "yes") {
            std::fs::remove_file(&conn)?;
            println!("deleted {}", conn.display());
        } else {
            println!("kept    {} — the connection stays DISABLED", conn.display());
        }
    }
    let refs = references(name);
    if refs.is_empty() {
        println!("nothing references it — removal complete");
    } else {
        println!("still referencing it — removal is complete only when these are gone:");
        for r in refs {
            println!("  - {r}");
        }
    }
    Ok(())
}

/// Every config surface that references a connection name, read raw (rm and
/// rotation guards must work even when config as a whole is invalid).
fn references(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let conn = crate::paths::connections_dir().join(format!("{name}.toml"));
    if conn.exists() {
        out.push(format!(
            "connection file {} — now DISABLED; delete it to drop the grant and exposure",
            conn.display()
        ));
    }
    for (worker, platform, path) in channel_binding_refs(name) {
        out.push(format!(
            "worker \"{worker}\" binds it as its {platform} channel ({})",
            path.display()
        ));
    }
    let scan_grants = |v: &toml::Value, source: &str, out: &mut Vec<String>| {
        for key in ["grant", "expose"] {
            for entry in v.get(key).and_then(|x| x.as_array()).into_iter().flatten() {
                let credential = match key {
                    "grant" => entry.get("inject").and_then(|i| i.get("credential")),
                    _ => entry.get("credential"),
                }
                .and_then(|x| x.as_str());
                if credential == Some(name) {
                    let label = entry
                        .get("name")
                        .and_then(|x| x.as_str())
                        .map(|n| format!(" \"{n}\""))
                        .unwrap_or_default();
                    out.push(format!("[[{key}]]{label} in {source} references it"));
                }
            }
        }
    };
    if let Some(org) = read_toml(&crate::paths::org_file()) {
        scan_grants(&org, "org.toml", &mut out);
    }
    for worker in crate::worker::names() {
        let spec = crate::paths::workers_dir()
            .join(&worker)
            .join("worker.toml");
        if let Some(w) = read_toml(&spec) {
            scan_grants(&w, &format!("workers/{worker}/worker.toml"), &mut out);
        }
    }
    out
}

/// (worker, platform, worker.toml path) for every `[channels]` binding that
/// names this credential.
fn channel_binding_refs(name: &str) -> Vec<(String, String, PathBuf)> {
    let mut out = Vec::new();
    for worker in crate::worker::names() {
        let spec = crate::paths::workers_dir()
            .join(&worker)
            .join("worker.toml");
        let Some(w) = read_toml(&spec) else { continue };
        let Some(channels) = w.get("channels").and_then(|c| c.as_table()) else {
            continue;
        };
        for (platform, v) in channels {
            if v.as_str() == Some(name) {
                out.push((worker.clone(), platform.clone(), spec.clone()));
            }
        }
    }
    out
}

fn read_toml(path: &std::path::Path) -> Option<toml::Value> {
    toml::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

// ── the catalog ──────────────────────────────────────────────────────────────

/// The registry, grouped by what connecting gives you.
fn print_catalog(registry: &serde_json::Map<String, Value>) -> Result<(), BErr> {
    let mut groups: [(&str, &str, Vec<String>); 3] = [
        (
            "capability",
            "Capabilities — a worker's box may act on the service",
            vec![],
        ),
        (
            "channel",
            "Channels — the worker talks there (bound in worker.toml)",
            vec![],
        ),
        (
            "model",
            "Models — grants inject them into model-API calls",
            vec![],
        ),
    ];
    let mut names: Vec<&String> = registry
        .iter()
        .filter(|(_, p)| !crate::credential::registry::is_hidden(p))
        .map(|(name, _)| name)
        .collect();
    names.sort();
    let width = names.iter().map(|n| n.len()).max().unwrap_or(0);
    let mut multi_use: Vec<&str> = vec![];
    for n in &names {
        let p = &registry[n.as_str()];
        let uses = crate::credential::registry::provider_uses(p);
        let mut placed = 0;
        for (key, _, lines) in groups.iter_mut() {
            if !uses.iter().any(|u| u == key) {
                continue;
            }
            placed += 1;
            let what = match *key {
                "capability" => {
                    let meta = &p["connection"];
                    let hosts: Vec<&str> = meta["hosts"]
                        .as_array()
                        .map(|a| a.iter().filter_map(Value::as_str).collect())
                        .unwrap_or_default();
                    format!(
                        "{} → {} ({})",
                        hosts.join(", "),
                        meta["env"].as_str().unwrap_or("?"),
                        auth_label(p)
                    )
                }
                _ => auth_label(p).to_string(),
            };
            lines.push(format!("  {n:width$}  {what}"));
        }
        if placed > 1 {
            multi_use.push(n);
        }
    }
    for (i, (_, title, lines)) in groups.iter().enumerate() {
        if lines.is_empty() {
            continue;
        }
        if i > 0 {
            println!();
        }
        println!("{title}:");
        for line in lines {
            println!("{line}");
        }
    }
    if !multi_use.is_empty() {
        println!(
            "\nOne connection, several uses: {} — add asks which to set up.",
            multi_use.join(", ")
        );
    }
    println!("\nConnect one: roster connection add <name> [--worker W].. [--org] [--use U]..");
    println!(
        "Anything else: roster connection add <name> --host <hostname>   (--declare for OAuth)"
    );
    Ok(())
}

fn auth_label(p: &Value) -> String {
    let one = |s: &str| -> &'static str {
        match s {
            "api_key" => "paste a key",
            "oauth" => "OAuth login",
            "slack" => "paste bot (+ app) tokens",
            "discord" => "paste a bot token",
            "smtp" => "SMTP details",
            _ => "?",
        }
    };
    match p.get("auth") {
        Some(Value::String(s)) => one(s).to_string(),
        // A list offers alternatives: the first is the default, the rest
        // reachable via --auth.
        Some(Value::Array(l)) => {
            let mut labels: Vec<String> = l
                .iter()
                .filter_map(Value::as_str)
                .map(|s| one(s).to_string())
                .collect();
            if labels.len() > 1 {
                let alternatives = l
                    .iter()
                    .skip(1)
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", --auth ");
                labels.truncate(1);
                format!("{} (or --auth {alternatives})", labels[0])
            } else {
                labels.pop().unwrap_or_else(|| "?".into())
            }
        }
        _ => "?".to_string(),
    }
}

pub fn catalog() -> Result<(), BErr> {
    print_catalog(&crate::credential::registry::registry_json())
}

// ── small helpers ────────────────────────────────────────────────────────────

fn ask_uses(service: &str, supported: &[String]) -> Result<Vec<String>, BErr> {
    let answer = crate::credential::connect::ask(&format!(
        "\"{service}\" supports: {}. set up which? (comma-separated, or \"all\") [all]: ",
        supported.join(", ")
    ))?;
    let answer = answer.trim();
    if answer.is_empty() || answer == "all" {
        return Ok(supported.to_vec());
    }
    let mut chosen = Vec::new();
    for u in answer
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        if !supported.iter().any(|s| s == u) {
            return Err(format!(
                "\"{u}\" is not a use of \"{service}\" (supports: {})",
                supported.join(", ")
            )
            .into());
        }
        if !chosen.iter().any(|c| c == u) {
            chosen.push(u.to_string());
        }
    }
    if chosen.is_empty() {
        return Err("no uses chosen".into());
    }
    Ok(chosen)
}

/// Ask with a default used when the reply is empty.
fn ask_default(question: &str, default: &str) -> Result<String, BErr> {
    let v = crate::credential::connect::ask(&format!("{question} [{default}]: "))?;
    let v = v.trim().to_string();
    Ok(if v.is_empty() { default.to_string() } else { v })
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

    #[test]
    fn binding_inserts_under_an_existing_channels_table() {
        let text = "name = \"yuko\"\n\n[channels]\ndiscord = \"discord\"\n";
        let out = upsert_channel_binding(text, "slack", "slack")
            .unwrap()
            .unwrap();
        assert_eq!(
            out,
            "name = \"yuko\"\n\n[channels]\nslack = \"slack\"\ndiscord = \"discord\"\n"
        );
    }

    #[test]
    fn binding_appends_the_table_when_absent() {
        let text = "name = \"yuko\"\nheartbeat = \"30m\"\n";
        let out = upsert_channel_binding(text, "discord", "disc-yuko")
            .unwrap()
            .unwrap();
        assert!(out.ends_with("[channels]\ndiscord = \"disc-yuko\"\n"));
        assert!(toml::from_str::<toml::Value>(&out).is_ok());
    }

    #[test]
    fn binding_is_idempotent_and_refuses_conflicts() {
        let text = "[channels]\ndiscord = \"discord\"\n";
        assert!(upsert_channel_binding(text, "discord", "discord")
            .unwrap()
            .is_none());
        let err = upsert_channel_binding(text, "discord", "other").unwrap_err();
        assert!(err.contains("already binds"));
    }

    #[test]
    fn binding_never_lands_in_a_later_table() {
        // The insert goes under [channels], not into a table that follows it.
        let text = "[channels]\n\n[memory]\nrecall = 4\n";
        let out = upsert_channel_binding(text, "slack", "s").unwrap().unwrap();
        let parsed: toml::Value = toml::from_str(&out).unwrap();
        assert_eq!(parsed["channels"]["slack"].as_str(), Some("s"), "{out}");
        assert!(parsed["memory"].get("slack").is_none());
    }
}
