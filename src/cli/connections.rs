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

    let named_explicitly = alias.is_some();
    let mut name = alias.unwrap_or_else(|| service.clone());
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

    // Edges: `add` connects the service at the roster level; --worker/--org
    // are sugar for the grant that follows (roster connection grant). Neither
    // flag = connected, granted to no one — grant it when ready.
    let known = match crate::config::snapshot() {
        Ok(c) => c.workers.clone(),
        Err(e) => return Err(format!("config must load before connecting a service:\n{e}").into()),
    };
    let mut edges: std::collections::BTreeMap<String, EdgeScope> = Default::default();
    if org {
        edges.insert("org".to_string(), EdgeScope::new());
    } else {
        for w in &workers {
            if !known.contains(w) {
                return Err(format!("no such worker \"{w}\" (have: {})", known.join(", ")).into());
            }
            edges.insert(w.clone(), EdgeScope::new());
        }
    }

    // The secret. Re-connecting rotates it in place; a credential that a
    // channel listener already consumes keeps its channel-only fields even
    // when this add is for another use (rotation must not break the bot).
    let channel_bound = !channel_binding_refs(&name).is_empty();
    let mut login_provider = registered
        .clone()
        .unwrap_or_else(|| serde_json::json!({ "auth": "api_key" }));
    login_provider["auth"] = serde_json::json!(chosen_auth);
    let cred =
        crate::credential::connect::login(&service, &login_provider, channel || channel_bound)
            .await?;
    // Name the connection after the bot the credential authenticates, when
    // the platform can say (probed with the fresh secret): each bot lands
    // under its own name — "discord-looper" — so a second bot never clobbers
    // the first, and re-adding the same bot naturally rotates in place.
    // --name overrides. Where no probe exists (or it fails), re-adding a
    // credential that live listeners consume is ambiguous — rotate this bot,
    // or connect a second one? — so ask rather than guess.
    if !named_explicitly {
        if let Some(bot) = bot_identity_slug(&service, &cred).await {
            let derived = format!("{service}-{bot}");
            if derived != name {
                println!("naming the connection \"{derived}\" after the bot (--name overrides)");
                name = derived;
            }
        } else if crate::credential::vault::get_credential(&name).is_some() {
            let bound = channel_binding_refs(&name);
            let interactive = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
            if !bound.is_empty() && interactive {
                let listeners: Vec<String> =
                    bound.iter().map(|(w, p, _)| format!("{w} ({p})")).collect();
                println!(
                    "credential \"{name}\" already exists — {} listens with it",
                    listeners.join(", ")
                );
                let answer = crate::credential::connect::ask(
                    "rotate its secret (same bot), or connect a NEW bot under its own name? [rotate/new]: ",
                )?;
                if matches!(answer.trim(), "new" | "n" | "N") {
                    let suggested = workers.first().map(|w| format!("{service}-{w}"));
                    let question = match &suggested {
                        Some(s) => format!("name for the new connection [{s}]: "),
                        None => format!("name for the new connection (e.g. {service}-<worker>): "),
                    };
                    let fresh = crate::credential::connect::ask(&question)?;
                    let fresh = fresh.trim().to_string();
                    name = if fresh.is_empty() {
                        suggested.ok_or("a new connection needs a name")?
                    } else {
                        fresh
                    };
                    if crate::credential::vault::get_credential(&name).is_some() {
                        return Err(format!(
                            "credential \"{name}\" already exists too — pick a fresh name"
                        )
                        .into());
                    }
                }
            }
        }
    }
    // A derived or asked-for name re-keys the connection file and rotation.
    let path = crate::paths::connections_dir().join(format!("{name}.toml"));
    let rotating = crate::credential::vault::get_credential(&name).is_some();
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
            &edges,
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
                    let scope = grants_summary(&conn.grants);
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
                let mut listener_bound = false;
                for (worker, platform, credential) in &c.listeners {
                    if credential == &name {
                        println!("listener: {platform} for {worker} with \"{name}\"");
                        listener_bound = true;
                    }
                }
                if listener_bound {
                    println!(
                        "note: the server connects listeners at start — restart a running \
                         server to bring this listener up"
                    );
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
    let verify_host = reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(String::from))
        .unwrap_or_default();
    for (k, v) in crate::credential::vault::render_injection(&cred, service, &verify_host) {
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
/// overwrites the admin's edits. The file is the roster-level object; the
/// `[grant.<worker>]` sections are its availability edges — none is a legal
/// resting state (connected, granted to no one).
#[allow(clippy::too_many_arguments)]
fn scaffold_connection(
    path: &std::path::Path,
    name: &str,
    service: &str,
    edges: &std::collections::BTreeMap<String, EdgeScope>,
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
    let grant_lines = if edges.is_empty() {
        String::new()
    } else {
        let mut out = String::new();
        for (who, scope) in edges {
            out.push('\n');
            out.push_str(&grant_section(who, scope));
        }
        out
    };
    std::fs::write(
        path,
        format!(
            "# Connection \"{name}\" — scaffolded by `roster connection add`, yours to edit.\n\
             # Compiles live into: an egress grant for these hosts/methods (evaluated\n\
             # before hand-written grants), credential injection in transit, and the env\n\
             # var below set in the box (to a sentinel; the secret never enters the box).\n\
             # Availability is per [grant.<worker>] edge: roster connection grant/revoke.\n\
             provider = \"{service}\"\n\
             hosts = [{hosts_line}]\n\
             methods = [{methods_line}]\n\
             env = \"{}\"\n\
             {inject_lines}{grant_lines}",
            toml_escape(env)
        ),
    )?;
    println!("created {}", path.display());
    if edges.is_empty() {
        println!(
            "granted to no one yet — make it available: roster connection grant {name} <worker>"
        );
    }
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
    let snippet = format!(
        "  [channels]\n  {platform} = \"{credential}\"\nthen restart the server (listeners connect at start)"
    );
    let known = crate::worker::names();
    if known.is_empty() {
        println!(
            "no workers yet — after `roster worker add <name>`, bind it in the worker's spec:\n{snippet}"
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
    // Refuse a binding config validation would reject, BEFORE writing it —
    // one bot cannot serve two listeners, and a broken spec on disk helps
    // nobody. The credential is already stored, so point at the way out.
    let taken: Vec<String> = channel_binding_refs(credential)
        .into_iter()
        .filter(|(w, _, _)| w != &worker)
        .map(|(w, _, _)| w)
        .collect();
    if !taken.is_empty() {
        println!(
            "NOT bound: {} already listens with credential \"{credential}\" — one bot cannot \
             serve two listeners.\nfor a second bot, connect it under its own name:\n  \
             roster connection add {platform} --name {platform}-{worker} --worker {worker}",
            taken.join(", ")
        );
        return Ok(());
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
    // A model connection stays org-wide by default: every box authenticates
    // through sentinel logins anyway, and a model nobody can call helps no one.
    let grant_lines = if workers.is_empty() {
        grant_section("org", &EdgeScope::new())
    } else {
        workers
            .iter()
            .map(|w| grant_section(w, &EdgeScope::new()))
            .collect::<Vec<_>>()
            .join("\n")
    };
    std::fs::write(
        &path,
        format!(
            "# Model connection — granted boxes may call {service}'s model API; the\n\
             # gateway injects the credential in transit. Hosts derive from the provider\n\
             # registry; all methods are allowed — narrow with hosts = [..] / methods = [..].\n\
             # ADMIN-OWNED after creation: edit this file or use grant/revoke.\n\
             provider = \"{service}\"\n\n\
             {grant_lines}"
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

/// The availability edges, human-readable: "org-wide", "dobby, kdemo",
/// "dobby (servers 1)" — each restricted edge carries a compact dim count.
fn grants_summary(
    grants: &std::collections::BTreeMap<String, std::collections::BTreeMap<String, Vec<String>>>,
) -> String {
    if grants.is_empty() {
        return "no one yet".into();
    }
    grants
        .iter()
        .map(|(who, scope)| {
            let label = if who == "org" { "org-wide" } else { who };
            if scope.is_empty() {
                label.to_string()
            } else {
                let dims = scope
                    .iter()
                    .map(|(d, ids)| format!("{d} {}", ids.len()))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{label} ({dims})")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

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
        let scope = grants_summary(&conn.grants);
        // A connection with no env exposure is a model connection: boxes
        // authenticate via sentinel logins, injection happens in transit.
        let is_model = conn.env.is_empty();
        rows.push(serde_json::json!({
            "name": conn.name,
            "use": if is_model { "model" } else { "capability" },
            "provider": conn.provider,
            "grants": conn.grants, "hosts": conn.hosts, "methods": conn.methods,
            "env": conn.env,
            "state": if !conn.enabled {
                "DISABLED (no secret)"
            } else if conn.grants.is_empty() {
                "ungranted"
            } else {
                "active"
            },
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
            crate::config::HostMountKind::Dir { rw } => (
                "host-dir",
                if *rw {
                    "rw".to_string()
                } else {
                    "ro".to_string()
                },
            ),
            crate::config::HostMountKind::Repo {
                gated,
                branch,
                write_from,
            } => (
                "host-repo",
                match (*gated, write_from.as_deref()) {
                    (true, Some(contract)) => format!("gated → {branch}, {contract}"),
                    (true, None) => format!("gated → {branch}"),
                    (false, _) => "ro".to_string(),
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

// ── grant / revoke ───────────────────────────────────────────────────────────

type EdgeScope = std::collections::BTreeMap<String, Vec<String>>;

/// Resolve `<worker>` / `--org` into the edge key, validated against the
/// live worker list.
fn resolve_edge_who(worker: Option<String>, org: bool) -> Result<String, BErr> {
    match (worker, org) {
        (Some(_), true) => Err("name a worker OR pass --org, not both".into()),
        (None, true) => Ok("org".to_string()),
        (None, false) => Err("name a worker, or --org for the fleet-wide edge".into()),
        (Some(w), false) => {
            if w == "org" {
                return Err("\"org\" is the fleet-wide edge — spell it --org".into());
            }
            let known = crate::worker::names();
            if !known.contains(&w) {
                return Err(format!("no such worker \"{w}\" (have: {})", known.join(", ")).into());
            }
            Ok(w)
        }
    }
}

/// `--restrict servers=111,222` flags → one edge scope. Dimensions are
/// validated against the provider's registry-declared `scope_dims` before
/// anything is written — the file never learns a dimension config would
/// reject.
fn parse_restrict_flags(flags: &[String], provider: &str) -> Result<EdgeScope, BErr> {
    let registry = crate::credential::registry::registry_json();
    let dims: Vec<String> = registry
        .get(provider)
        .and_then(|p| p.get("scope_dims"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let mut scope = EdgeScope::new();
    for flag in flags {
        let Some((dim, ids)) = flag.split_once('=') else {
            return Err(
                format!("--restrict takes <dimension>=<id>[,<id>..] — got \"{flag}\"").into(),
            );
        };
        let dim = dim.trim();
        // The pre-rename name of the surfaces dim still parses; the file is
        // written in the current vocabulary.
        let dim = if dim == "channels" && dims.iter().any(|d| d == "surfaces") {
            "surfaces"
        } else {
            dim
        };
        if !dims.iter().any(|d| d == dim) {
            let declared = if dims.is_empty() {
                "it declares none".to_string()
            } else {
                format!("it declares: {}", dims.join(", "))
            };
            return Err(format!(
                "provider \"{provider}\" has no scope dimension \"{dim}\" ({declared})"
            )
            .into());
        }
        let classes: Vec<String> = registry
            .get(provider)
            .and_then(|p| p.get("surface_classes"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let entry = scope.entry(dim.to_string()).or_default();
        for id in ids.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if crate::config::SurfaceClass::parse(id).is_some() {
                if dim != "surfaces" {
                    return Err(format!(
                        "\"{id}\" is a surface class — it belongs in --restrict surfaces={id}"
                    )
                    .into());
                }
                if !classes.iter().any(|c| c == id) {
                    return Err(format!(
                        "provider \"{provider}\" does not classify \"{id}\" surfaces"
                    )
                    .into());
                }
            }
            if !entry.iter().any(|e| e == id) {
                entry.push(id.to_string());
            }
        }
        if scope[dim].is_empty() {
            return Err(format!("--restrict {dim}= names no ids").into());
        }
    }
    Ok(scope)
}

/// One `[grant.<who>]` section, rendered.
fn grant_section(who: &str, scope: &EdgeScope) -> String {
    let mut out = format!("[grant.{who}]\n");
    for (dim, ids) in scope {
        out.push_str(&format!(
            "{dim} = [{}]\n",
            ids.iter()
                .map(|i| format!("\"{i}\""))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    out
}

/// The line span of a `[section]` in TOML text: header line index and the
/// exclusive end (the next `[` header or EOF). Only finds plain headers —
/// dotted or inline layouts return None and the caller refuses the edit.
fn section_span(lines: &[&str], header: &str) -> Option<(usize, usize)> {
    let start = lines.iter().position(|l| {
        let t = l.trim();
        t == header
            || t.strip_prefix(header)
                .is_some_and(|rest| rest.trim_start().starts_with('#'))
    })?;
    let end = lines[start + 1..]
        .iter()
        .position(|l| l.trim_start().starts_with('['))
        .map(|i| start + 1 + i)
        .unwrap_or(lines.len());
    Some((start, end))
}

/// Rewrite a legacy `workers = [..]` / `scope = "org"` / `[restrict]` file
/// into `[grant.<who>]` edges — same meaning, new shape. Ok(None) = nothing
/// legacy here. The parse happens first, so values survive the text surgery.
fn migrate_legacy_grants(text: &str) -> Result<Option<String>, String> {
    let parsed: toml::Value =
        toml::from_str(text).map_err(|e| format!("connection file is invalid: {e}"))?;
    let strings = |key: &str| -> Option<Vec<String>> {
        parsed.get(key).and_then(|x| x.as_array()).map(|a| {
            a.iter()
                .filter_map(|s| s.as_str())
                .map(str::to_string)
                .collect()
        })
    };
    let workers = strings("workers").or_else(|| strings("imps"));
    let org_scoped = parsed.get("scope").and_then(|x| x.as_str()) == Some("org");
    let has_restrict = parsed.get("restrict").is_some();
    if workers.is_none() && !org_scoped && !has_restrict {
        return Ok(None);
    }
    let shared: EdgeScope = parsed
        .get("restrict")
        .and_then(|r| r.as_table())
        .map(|t| {
            t.iter()
                .filter_map(|(dim, val)| {
                    let ids: Vec<String> = val
                        .as_array()?
                        .iter()
                        .filter_map(|s| s.as_str())
                        .map(str::to_string)
                        .collect();
                    Some((dim.clone(), ids))
                })
                .collect()
        })
        .unwrap_or_default();

    let lines: Vec<&str> = text.lines().collect();
    let mut drop: Vec<bool> = vec![false; lines.len()];
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim_start();
        let legacy_key = ["workers", "imps", "scope"].iter().any(|k| {
            t.strip_prefix(k)
                .is_some_and(|r| r.trim_start().starts_with('='))
        });
        if legacy_key {
            if (t.starts_with("workers") || t.starts_with("imps")) && !l.contains(']') {
                return Err(
                    "workers = [..] spans several lines — edit the file to [grant.<worker>] form by hand"
                        .into(),
                );
            }
            drop[i] = true;
        }
    }
    if has_restrict {
        let Some((start, end)) = section_span(&lines, "[restrict]") else {
            return Err(
                "has a [restrict] layout this tool cannot edit — migrate it by hand".into(),
            );
        };
        for d in drop.iter_mut().take(end).skip(start) {
            *d = true;
        }
    }
    let mut out: String = lines
        .iter()
        .enumerate()
        .filter(|(i, _)| !drop[*i])
        .map(|(_, l)| format!("{l}\n"))
        .collect();
    while out.ends_with("\n\n") {
        out.pop();
    }
    let edges: Vec<String> = match &workers {
        Some(list) => list.clone(),
        None => vec!["org".to_string()],
    };
    for who in edges {
        out.push('\n');
        out.push_str(&grant_section(&who, &shared));
    }
    Ok(Some(out))
}

/// Upsert a `[grant.<who>]` section in connection-file text. The result is
/// parse-checked before it is returned — surgery that corrupts the file is a
/// bug here, never a broken config there.
fn upsert_grant_edge(text: &str, who: &str, scope: &EdgeScope) -> Result<(String, bool), String> {
    let parsed: toml::Value =
        toml::from_str(text).map_err(|e| format!("connection file is invalid: {e}"))?;
    let exists = parsed.get("grant").and_then(|g| g.get(who)).is_some();
    let lines: Vec<&str> = text.lines().collect();
    let section = grant_section(who, scope);
    let out = if exists {
        let Some((start, end)) = section_span(&lines, &format!("[grant.{who}]")) else {
            return Err(format!(
                "has a [grant] layout this tool cannot edit — set [grant.{who}] by hand"
            ));
        };
        let mut out: String = lines[..start].iter().map(|l| format!("{l}\n")).collect();
        out.push_str(&section);
        // Keep one blank line before a following section.
        if end < lines.len() {
            out.push('\n');
        }
        out.push_str(
            &lines[end..]
                .iter()
                .map(|l| format!("{l}\n"))
                .collect::<String>(),
        );
        out
    } else {
        let mut out = text.to_string();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
        out.push_str(&section);
        out
    };
    toml::from_str::<toml::Value>(&out).map_err(|e| format!("edit would corrupt the file: {e}"))?;
    Ok((out, exists))
}

/// Remove a `[grant.<who>]` section. Err when there is no such edge.
fn remove_grant_edge(text: &str, who: &str) -> Result<String, String> {
    let parsed: toml::Value =
        toml::from_str(text).map_err(|e| format!("connection file is invalid: {e}"))?;
    if parsed.get("grant").and_then(|g| g.get(who)).is_none() {
        let have: Vec<String> = parsed
            .get("grant")
            .and_then(|g| g.as_table())
            .map(|t| t.keys().cloned().collect())
            .unwrap_or_default();
        let have = if have.is_empty() {
            "none".to_string()
        } else {
            have.join(", ")
        };
        return Err(format!("no [grant.{who}] edge (edges: {have})"));
    }
    let lines: Vec<&str> = text.lines().collect();
    let Some((start, end)) = section_span(&lines, &format!("[grant.{who}]")) else {
        return Err(format!(
            "has a [grant] layout this tool cannot edit — remove [grant.{who}] by hand"
        ));
    };
    let mut out: String = lines
        .iter()
        .enumerate()
        .filter(|(i, _)| *i < start || *i >= end)
        .map(|(_, l)| format!("{l}\n"))
        .collect();
    while out.ends_with("\n\n") {
        out.pop();
    }
    toml::from_str::<toml::Value>(&out).map_err(|e| format!("edit would corrupt the file: {e}"))?;
    Ok(out)
}

/// `roster connection grant <name> <worker> [--restrict dim=ids] | --org` —
/// make a roster-level connection available to a worker. The restriction
/// rides on the edge: each worker's grant carries its own scope.
pub fn grant(
    name: &str,
    worker: Option<String>,
    org: bool,
    restrict_flags: &[String],
) -> Result<(), BErr> {
    let who = resolve_edge_who(worker, org)?;
    let path = crate::paths::connections_dir().join(format!("{name}.toml"));
    let registry = crate::credential::registry::registry_json();

    // The provider governs which restrict dimensions exist: from the file
    // when there is one, else the registry entry the file will scaffold from.
    // Host mounts have no provider and no dimensions — their edges are
    // membership only.
    let existing_text = std::fs::read_to_string(&path).ok();
    let is_mount = existing_text
        .as_deref()
        .and_then(|t| toml::from_str::<toml::Value>(t).ok())
        .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_string))
        .is_some_and(|k| k == "host-dir" || k == "host-repo");
    let scope = if is_mount {
        if !restrict_flags.is_empty() {
            return Err("host mounts take no --restrict — an edge is membership only".into());
        }
        EdgeScope::new()
    } else {
        let provider = match &existing_text {
            Some(text) => toml::from_str::<toml::Value>(text)
                .map_err(|e| format!("{}: {e}", path.display()))?
                .get("provider")
                .and_then(|p| p.as_str())
                .map(str::to_string)
                .ok_or_else(|| format!("{} names no provider", path.display()))?,
            None => {
                if !registry.contains_key(name) {
                    return Err(format!(
                        "no connection \"{name}\" — connect it first: roster connection add {name}"
                    )
                    .into());
                }
                name.to_string()
            }
        };
        parse_restrict_flags(restrict_flags, &provider)?
    };

    match existing_text {
        Some(text) => {
            let (text, migrated) = match migrate_legacy_grants(&text)
                .map_err(|e| format!("{}: {e}", path.display()))?
            {
                Some(t) => (t, true),
                None => (text, false),
            };
            let (out, replaced) = upsert_grant_edge(&text, &who, &scope)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            std::fs::write(&path, out)?;
            if migrated {
                println!(
                    "migrated {} to per-worker [grant.*] edges (same meaning, new shape)",
                    path.display()
                );
            }
            println!(
                "{} [grant.{who}] in {}",
                if replaced { "updated" } else { "added  " },
                path.display()
            );
        }
        None => {
            // Scaffold the roster-level file this grant hangs off — identity
            // from the registry, exactly as `add` would have written it.
            let entry = &registry[name];
            let is_model = entry.get("model_hosts").is_some();
            let meta = entry.get("connection");
            let hosts: Vec<String> = meta
                .and_then(|m| m["hosts"].as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_else(|| match name {
                    "discord" => vec!["discord.com".to_string()],
                    _ => Vec::new(),
                });
            if !is_model && hosts.is_empty() {
                // A channel-use credential (smtp, a bot token) is consumed
                // host-side by its listener or executor — there is no box
                // egress to grant. Say what the operator actually wants
                // instead of "connect it first".
                let uses = crate::credential::registry::provider_uses(entry);
                if !uses.is_empty() && uses.iter().all(|u| u == "channel") {
                    let follow_up = if name == "smtp" {
                        "workers send email through the \"email-send\" action — grant it in org.toml:\n  \
                         [[action]]\n  scope = \"org/<worker>\"   # or omit for org-wide\n  \
                         name = \"email-send\"\n  executor = \"email\"\n  trust = \"gate\""
                            .to_string()
                    } else {
                        format!(
                            "a worker talks through it via its [channels] binding: \
                             roster connection add {name} --worker <w>"
                        )
                    };
                    return Err(format!(
                        "\"{name}\" is a channel credential — the host consumes it (listener/executor), \
                         so there is no box egress to grant.\n{follow_up}"
                    )
                    .into());
                }
                return Err(format!(
                    "\"{name}\" declares no hosts to grant — connect it first: roster connection add {name}"
                )
                .into());
            }
            let env = meta
                .and_then(|m| m["env"].as_str())
                .map(str::to_string)
                .unwrap_or_else(|| default_env(name));
            let mut edges = std::collections::BTreeMap::new();
            edges.insert(who.clone(), scope.clone());
            if is_model {
                std::fs::create_dir_all(crate::paths::connections_dir())?;
                std::fs::write(
                    &path,
                    format!(
                        "# Model connection — granted boxes may call {name}'s model API; the\n\
                         # gateway injects the credential in transit.\n\
                         provider = \"{name}\"\n\n{}",
                        grant_section(&who, &scope)
                    ),
                )?;
                println!("created {}", path.display());
            } else {
                // A missing file means name == provider (registry-resolved).
                scaffold_connection(
                    &path,
                    name,
                    name,
                    &edges,
                    &hosts,
                    &["*".to_string()],
                    &env,
                    &None,
                )?;
            }
        }
    }

    // The compiled result — same follow-through as add: warnings out loud,
    // then this connection's live state.
    match crate::config::load() {
        Ok(c) => {
            for w in &c.warnings {
                println!("warning: {w}");
            }
            if let Some(conn) = c.connections.iter().find(|c| c.name == name) {
                println!("granted: {} → {}", conn.name, grants_summary(&conn.grants));
            }
            if let Some(m) = c.host_mounts.iter().find(|m| m.name == name) {
                let audience = match &m.workers {
                    None => "org-wide".to_string(),
                    Some(l) if l.is_empty() => "no one yet".to_string(),
                    Some(l) => l.join(", "),
                };
                println!("granted: {} (mount) → {}", m.name, audience);
            }
            let listeners: Vec<String> = c
                .listeners
                .iter()
                .filter(|(_, _, credential)| credential == name)
                .map(|(w, platform, _)| format!("{platform} for {w}"))
                .collect();
            if !listeners.is_empty() {
                println!(
                    "listening: {} — the edge scopes the listener too",
                    listeners.join(", ")
                );
            }
        }
        Err(errors) => {
            for e in &errors {
                eprintln!("config: {e}");
            }
            return Err(format!(
                "{} config error(s) — fix before the grant is live",
                errors.len()
            )
            .into());
        }
    }
    Ok(())
}

/// `roster connection revoke <name> <worker> | --org` — withdraw an edge.
/// The connection itself stays; a channel binding consuming the credential in
/// that worker's spec is removed with the edge (grant wrote it, revoke owns
/// it).
pub fn revoke(name: &str, worker: Option<String>, org: bool) -> Result<(), BErr> {
    let who = resolve_edge_who(worker, org)?;
    let path = crate::paths::connections_dir().join(format!("{name}.toml"));
    let text = std::fs::read_to_string(&path)
        .map_err(|_| format!("no connection file for \"{name}\" — nothing to revoke"))?;
    let (text, migrated) =
        match migrate_legacy_grants(&text).map_err(|e| format!("{}: {e}", path.display()))? {
            Some(t) => (t, true),
            None => (text, false),
        };
    let mut out = remove_grant_edge(&text, &who).map_err(|e| format!("{}: {e}", path.display()))?;
    let parsed = toml::from_str::<toml::Value>(&out).ok();
    let no_edges_left = parsed
        .as_ref()
        .and_then(|v| {
            v.get("grant")
                .and_then(|g| g.as_table())
                .map(|t| t.is_empty())
        })
        .unwrap_or(true);
    let is_mount = parsed
        .as_ref()
        .and_then(|v| v.get("kind").and_then(|k| k.as_str()))
        .is_some_and(|k| k == "host-dir" || k == "host-repo");
    // A mount must declare its audience to parse — an explicit empty [grant]
    // says "granted to no one". Service files need no marker: no grant syntax
    // already means exactly that.
    if no_edges_left && is_mount && !out.lines().any(|l| l.trim() == "[grant]") {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("\n[grant]\n");
    }
    std::fs::write(&path, &out)?;
    if migrated {
        println!(
            "migrated {} to per-worker [grant.*] edges (same meaning, new shape)",
            path.display()
        );
    }
    println!("revoked [grant.{who}] in {}", path.display());

    // The worker's listener binding dies with its edge.
    if who != "org" {
        let spec = crate::paths::workers_dir().join(&who).join("worker.toml");
        if let Ok(text) = std::fs::read_to_string(&spec) {
            if let Some((new_text, platform)) = remove_channel_binding(&text, name) {
                std::fs::write(&spec, new_text)?;
                println!("unbound {platform} = \"{name}\" in {}", spec.display());
            }
        }
    }

    let remaining: Vec<String> = toml::from_str::<toml::Value>(&out)
        .ok()
        .and_then(|v| {
            v.get("grant")
                .and_then(|g| g.as_table())
                .map(|t| t.keys().cloned().collect())
        })
        .unwrap_or_default();
    if remaining.is_empty() {
        println!(
            "no edges left — \"{name}\" stays connected, granted to no one (remove it: roster connection rm {name})"
        );
    } else {
        println!("edges left: {}", remaining.join(", "));
    }
    Ok(())
}

/// Drop a `<platform> = "<credential>"` line from a worker.toml `[channels]`
/// table. Returns the new text and the platform unbound, or None when the
/// credential isn't bound there.
fn remove_channel_binding(text: &str, credential: &str) -> Option<(String, String)> {
    let parsed: toml::Value = toml::from_str(text).ok()?;
    let channels = parsed.get("channels")?.as_table()?;
    let platform = channels
        .iter()
        .find(|(_, v)| v.as_str() == Some(credential))
        .map(|(k, _)| k.clone())?;
    let needle = platform.to_string();
    let mut removed = false;
    let out: String = text
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            let hit = !removed
                && t.strip_prefix(needle.as_str())
                    .is_some_and(|r| r.trim_start().starts_with('='))
                && l.contains(credential);
            if hit {
                removed = true;
            }
            !hit
        })
        .map(|l| format!("{l}\n"))
        .collect();
    if !removed || toml::from_str::<toml::Value>(&out).is_err() {
        return None;
    }
    Some((out, platform))
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
/// The authenticated bot's own name, slugged for use in a connection name —
/// discord asks `users/@me`, slack asks `auth.test`. None for services with
/// no identity probe, or when the probe fails (an offline add still works).
async fn bot_identity_slug(service: &str, cred: &Value) -> Option<String> {
    let token = |field: &str| {
        cred.get(field)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    let raw = match service {
        "discord" => crate::channel::discord::bot_username(token("token")?)
            .await
            .ok()?,
        "slack" => crate::channel::slack::bot_username(token("bot_token")?)
            .await
            .ok()?,
        _ => return None,
    };
    slug(&raw)
}

/// A bot name as a bare identifier: lowercased, runs of anything else
/// collapsed to single dashes ("Looper Bot" → "looper-bot").
fn slug(raw: &str) -> Option<String> {
    let mut out = String::new();
    for c in raw.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').to_string();
    (!out.is_empty()).then_some(out)
}

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
    fn bot_slugs_are_bare_identifiers() {
        assert_eq!(slug("looper"), Some("looper".into()));
        assert_eq!(slug("Looper Bot"), Some("looper-bot".into()));
        assert_eq!(slug("Zoë's  Bot!"), Some("zo-s-bot".into()));
        assert_eq!(slug("under_score"), Some("under_score".into()));
        assert_eq!(slug("---"), None);
        assert_eq!(slug(""), None);
    }

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
    fn legacy_grants_migrate_and_edges_upsert_and_remove() {
        // workers + [restrict] → per-worker edges carrying the shared scope.
        let text = "# keep me\nprovider = \"discord\"\nworkers = [\"dobby\", \"kdemo\"]\n\
                    hosts = [\"discord.com\"]\nenv = \"D\"\n\n[restrict]\nservers = [\"999\"]\n";
        let migrated = migrate_legacy_grants(text).unwrap().unwrap();
        assert!(migrated.contains("# keep me"));
        assert!(!migrated.contains("workers ="));
        assert!(!migrated.contains("[restrict]"));
        assert!(migrated.contains("[grant.dobby]\nservers = [\"999\"]"));
        assert!(migrated.contains("[grant.kdemo]\nservers = [\"999\"]"));
        // Already-migrated text is a no-op.
        assert!(migrate_legacy_grants(&migrated).unwrap().is_none());

        // Upsert replaces an edge in place; remove drops it whole.
        let mut scope = EdgeScope::new();
        scope.insert("channels".into(), vec!["1".into(), "2".into()]);
        let (out, replaced) = upsert_grant_edge(&migrated, "dobby", &scope).unwrap();
        assert!(replaced);
        assert!(out.contains("[grant.dobby]\nchannels = [\"1\", \"2\"]"));
        assert!(out.contains("[grant.kdemo]\nservers = [\"999\"]"));
        let out = remove_grant_edge(&out, "kdemo").unwrap();
        assert!(!out.contains("[grant.kdemo]"));
        assert!(out.contains("[grant.dobby]"));
        assert!(remove_grant_edge(&out, "kdemo").is_err());
    }

    #[test]
    fn channel_binding_removal_is_surgical() {
        let text = "name = \"dobby\"\n\n[channels]\ndiscord = \"discord\"\nslack = \"slack\"\n\n\
                    [[budget.limit]]\ncurrency = \"model_calls\"\nwindow = \"day\"\nmax = 5000\n";
        let (out, platform) = remove_channel_binding(text, "discord").unwrap();
        assert_eq!(platform, "discord");
        assert!(!out.contains("discord"));
        assert!(out.contains("slack = \"slack\""));
        assert!(out.contains("[[budget.limit]]"));
        assert!(remove_channel_binding(text, "nope").is_none());
    }

    #[test]
    fn binding_inserts_under_an_existing_channels_table() {
        let text = "name = \"dobby\"\n\n[channels]\ndiscord = \"discord\"\n";
        let out = upsert_channel_binding(text, "slack", "slack")
            .unwrap()
            .unwrap();
        assert_eq!(
            out,
            "name = \"dobby\"\n\n[channels]\nslack = \"slack\"\ndiscord = \"discord\"\n"
        );
    }

    #[test]
    fn binding_appends_the_table_when_absent() {
        let text = "name = \"dobby\"\nheartbeat = \"30m\"\n";
        let out = upsert_channel_binding(text, "discord", "disc-dobby")
            .unwrap()
            .unwrap();
        assert!(out.ends_with("[channels]\ndiscord = \"disc-dobby\"\n"));
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
