//! Live config. `org.toml` + `workers/*/worker.toml` parse straight into the
//! gateway's OWN types (schema::Policy, budget::BudgetPolicy, …), scope-tagged
//! in memory — there is no compile step and no intermediate artifact. This is
//! still the payoff of one language, one schema (D20): what validates is
//! literally what runs.
//!
//! Consumers call `snapshot()` (mtime-fingerprint cache, so admin edits are
//! live). Invalid config fails closed: the gateway denies, dispatch pauses,
//! `server start` refuses to boot — and `roster server validate` prints every
//! error. `load()` is side-effect free.

use crate::action::ActionPolicy;
use crate::gateway::budget::BudgetPolicy;
use crate::gateway::schema::Policy;
use crate::paths;
use crate::worker::context::{CompiledContextPolicy, ContextPolicy};
use crate::worker::memory::{CompiledMemoryPolicy, MemoryPolicy};
use crate::worker::storage::{CompiledStoragePolicy, StoragePolicy};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

/// A service connection (`connections/<name>.toml`): one intent — "this
/// worker may act on that service" — compiled into a grant with injection,
/// an env exposure, and a provider template, all keyed by one name that is
/// also the vault credential. Missing secret ⇒ disabled with a warning, not
/// a config failure (nothing forwards a sentinel either way).
#[derive(Clone, Debug)]
pub struct Connection {
    pub name: String,
    pub provider: String,
    /// None = org-wide; Some = these workers only.
    pub workers: Option<Vec<String>>,
    pub hosts: Vec<String>,
    pub methods: Vec<String>,
    pub env: String,
    /// Secret present in the vault?
    pub enabled: bool,
}

#[derive(Clone, Debug)]
pub struct Expose {
    /// "org" or "org/<worker>" — which workers see this env var.
    pub scope: String,
    /// Vault credential name (must exist — fail closed, like listeners).
    pub credential: String,
    /// The env var set in the box (to the sentinel, never the real value).
    pub env: String,
}

/// `[box]` in org.toml — the container's hardening and resource envelope, plus
/// warm-session wall-clock ceilings. All optional; the defaults below apply
/// when the section (or a key) is absent. A bad value falls back to the default
/// rather than failing the whole config (these are operational knobs, not
/// grants).
#[derive(Clone, Debug)]
pub struct BoxPolicy {
    /// `--pids-limit` (fork-bomb guard). <= 0 disables the flag.
    pub pids_limit: i64,
    /// `--memory` (e.g. "4g"). None = unlimited (a host-DoS risk; set it).
    pub memory: Option<String>,
    /// `--cpus` (e.g. "2"). None = unlimited.
    pub cpus: Option<String>,
    /// Install host firewall rules pinning the locked network's egress to the
    /// gateway host:port only (F3). Off by default: it needs root/CAP_NET_ADMIN,
    /// and when off the runner warns that host-local services stay reachable.
    pub egress_lockdown: bool,
    /// Hard wall-clock ceiling for a whole warm session (minutes).
    pub session_ceiling_min: f64,
    /// Hard wall-clock ceiling for a single warm-session turn (minutes) — the
    /// bound the idle timer can't provide, since a wedged turn never goes idle.
    pub turn_ceiling_min: f64,
}

impl Default for BoxPolicy {
    fn default() -> Self {
        Self {
            pids_limit: 1024,
            memory: None,
            cpus: None,
            egress_lockdown: false,
            session_ceiling_min: 60.0,
            turn_ceiling_min: 15.0,
        }
    }
}

fn parse_box_policy(v: Option<&toml::Value>) -> BoxPolicy {
    let d = BoxPolicy::default();
    let Some(v) = v else { return d };
    let num = |key: &str| -> Option<f64> {
        v.get(key)
            .and_then(|x| x.as_float().or_else(|| x.as_integer().map(|i| i as f64)))
    };
    let string_like = |key: &str| -> Option<String> {
        v.get(key).and_then(|x| {
            x.as_str()
                .map(str::to_string)
                .or_else(|| x.as_integer().map(|i| i.to_string()))
                .or_else(|| x.as_float().map(|f| f.to_string()))
        })
    };
    BoxPolicy {
        pids_limit: v
            .get("pids_limit")
            .and_then(|x| x.as_integer())
            .unwrap_or(d.pids_limit),
        memory: string_like("memory"),
        cpus: string_like("cpus"),
        egress_lockdown: v
            .get("egress_lockdown")
            .and_then(|x| x.as_bool())
            .unwrap_or(d.egress_lockdown),
        session_ceiling_min: num("session_ceiling_min")
            .filter(|m| *m > 0.0)
            .unwrap_or(d.session_ceiling_min),
        turn_ceiling_min: num("turn_ceiling_min")
            .filter(|m| *m > 0.0)
            .unwrap_or(d.turn_ceiling_min),
    }
}

pub struct Loaded {
    pub policy: Policy,
    pub budget: BudgetPolicy,
    pub actions: ActionPolicy,
    /// worker → heartbeat interval string ("every 30m" default; "off"
    /// disables). The TMS keeps each worker's system template in line.
    pub heartbeats: std::collections::HashMap<String, String>,
    pub memory: CompiledMemoryPolicy,
    pub context: CompiledContextPolicy,
    pub storage: CompiledStoragePolicy,
    /// (worker, platform, vault credential) — `server start` starts one
    /// listener each. Platforms: "discord", "slack".
    pub listeners: Vec<(String, String, String)>,
    /// `[[expose]]` — env vars set in the box to the sentinel; the gateway's
    /// per-grant injection swaps in the real credential in transit, only on
    /// requests the grant's scope allows. Leaking the box env leaks nothing.
    /// Includes the exposures compiled from enabled connections.
    pub exposes: Vec<Expose>,
    /// Service connections, for `connection ls` and the wizard.
    pub connections: Vec<Connection>,
    /// Non-fatal conditions (e.g. a disabled connection) — printed by
    /// `validate` and `server start`, never fail-closed.
    pub warnings: Vec<String>,
    pub workers: Vec<String>,
    /// `[engine] dir` in org.toml — a dev checkout mounted read-only over the
    /// engine baked into the roster-box image. Unset (the default) runs the
    /// baked engine.
    pub engine_dir: Option<PathBuf>,
    /// `[box]` — container hardening/resource envelope and session ceilings.
    pub box_policy: BoxPolicy,
}

/// Parse and validate everything, collecting every error (not just the first).
/// Side-effect free — this is also `roster server validate`.
pub fn load() -> Result<Loaded, Vec<String>> {
    let mut errors: Vec<String> = Vec::new();
    let org_path = paths::org_file();
    let org = match read_toml(&org_path) {
        Ok(v) => v,
        Err(e) => {
            errors.push(format!("{}: {e}", org_path.display()));
            toml::Value::Table(Default::default())
        }
    };

    let mut rules: Vec<Value> = Vec::new();
    let mut limits: Vec<Value> = Vec::new();
    let mut actions: Vec<Value> = Vec::new();
    let mut trust: Vec<Value> = Vec::new();
    let mut heartbeats: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut listeners: Vec<(String, String, String)> = Vec::new();
    let mut exposes: Vec<Expose> = Vec::new();
    let mut workers: Vec<String> = Vec::new();

    let default_memory = memory_policy(org.get("memory"), None).unwrap_or_else(|e| {
        errors.push(format!("org.toml [memory]: {e}"));
        MemoryPolicy::default()
    });
    let default_context = context_policy(org.get("context"), None).unwrap_or_else(|e| {
        errors.push(format!("org.toml [context]: {e}"));
        ContextPolicy::default()
    });
    let default_storage = storage_policy(&org, None).unwrap_or_else(|e| {
        errors.push(format!("org.toml [knowledge]: {e}"));
        StoragePolicy::default()
    });
    let mut worker_memory = std::collections::HashMap::new();
    let mut worker_context = std::collections::HashMap::new();
    let mut worker_storage = std::collections::HashMap::new();

    for g in array(&org, "grant") {
        rules.push(with_scope(g, "org"));
    }
    for a in array(&org, "action") {
        actions.push(with_scope(a, "org"));
    }
    for t in array(&org, "trust") {
        trust.push(with_scope(t, "org"));
    }
    let org_budget = org.get("budget");
    for l in org_budget.map(|b| array(b, "limit")).unwrap_or_default() {
        limits.push(with_scope(l, "org"));
    }
    for e in array(&org, "expose") {
        parse_expose(e, "org", "org.toml", &mut exposes, &mut errors);
    }

    let engine_dir = org
        .get("engine")
        .and_then(|e| e.get("dir"))
        .and_then(|v| v.as_str())
        .map(PathBuf::from);

    let box_policy = parse_box_policy(org.get("box"));

    let workers_dir = paths::workers_dir();
    if workers_dir.is_dir() {
        let mut names: Vec<String> = std::fs::read_dir(&workers_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for name in names {
            let spec = workers_dir.join(&name).join("worker.toml");
            if !spec.exists() {
                continue;
            }
            let w = match read_toml(&spec) {
                Ok(v) => v,
                Err(e) => {
                    errors.push(format!("{}: {e}", spec.display()));
                    continue;
                }
            };
            let declared = w.get("name").and_then(|v| v.as_str());
            if declared != Some(name.as_str()) {
                errors.push(format!(
                    "{}: name {declared:?} != folder \"{name}\"",
                    spec.display()
                ));
                continue;
            }
            let scope = format!("org/{name}");
            workers.push(name.clone());
            match memory_policy(w.get("memory"), Some(&default_memory)) {
                Ok(p) => {
                    worker_memory.insert(name.clone(), p);
                }
                Err(e) => errors.push(format!("{name} [memory]: {e}")),
            }
            match context_policy(w.get("context"), Some(&default_context)) {
                Ok(p) => {
                    worker_context.insert(name.clone(), p);
                }
                Err(e) => errors.push(format!("{name} [context]: {e}")),
            }
            match storage_policy(&w, Some(&default_storage)) {
                Ok(storage) => {
                    match crate::worker::storage::validate_worker_overlay(
                        &default_storage,
                        &storage,
                    ) {
                        Ok(()) => {
                            worker_storage.insert(name.clone(), storage);
                        }
                        Err(e) => errors.push(format!("{name} [knowledge]: {e}")),
                    }
                }
                Err(e) => errors.push(format!("{name} [knowledge]: {e}")),
            }
            for g in array(&w, "grant") {
                rules.push(with_scope(g, &scope));
            }
            for a in array(&w, "action") {
                actions.push(with_scope(a, &scope));
            }
            for t in array(&w, "trust") {
                trust.push(with_scope(t, &scope));
            }
            // [[trigger]] retired (docs/work.md): periodic
            // invocation is the heartbeat; other cadences are the worker's own
            // recurring templates in its task partition.
            if w.get("trigger").is_some() {
                errors.push(format!(
                    "{name}: [[trigger]] has retired — set heartbeat = \"30m\" and move cadences into the worker's recurring tasks (talk to it, or roster worker task ls)"
                ));
            }
            let heartbeat = w
                .get("heartbeat")
                .and_then(|v| v.as_str())
                .unwrap_or("every 30m")
                .to_string();
            if heartbeat != "off" && crate::work::tms::parse_interval(&heartbeat).is_none() {
                errors.push(format!(
                    "{name}: heartbeat must be an interval (\"every 30m\") or \"off\", not \"{heartbeat}\""
                ));
            }
            heartbeats.insert(name.clone(), heartbeat);
            if let Some(b) = w.get("budget") {
                for l in array(b, "limit") {
                    limits.push(with_scope(l, &scope));
                }
            }
            for e in array(&w, "expose") {
                parse_expose(e, &scope, &name, &mut exposes, &mut errors);
            }
            // [channels] — which vault credential each of this worker's
            // inbound edges uses. Two listeners on one credential would
            // double-file every message, so that is a validation error, not a
            // runtime surprise.
            for platform in ["discord", "slack"] {
                if let Some(credential) = w
                    .get("channels")
                    .and_then(|c| c.get(platform))
                    .and_then(|v| v.as_str())
                {
                    if let Some((taken, _, _)) = listeners.iter().find(|(_, _, c)| c == credential)
                    {
                        errors.push(format!(
                            "workers {taken} and {name} both listen with credential \"{credential}\" — one bot cannot serve two listeners"
                        ));
                    } else {
                        listeners.push((
                            name.clone(),
                            platform.to_string(),
                            credential.to_string(),
                        ));
                    }
                }
            }
        }
    }

    // Service connections (connections/<name>.toml). Their grants are spliced
    // BEFORE all hand-written grants: first-match-wins, and a connection is
    // host-specific by construction, so it must not be shadowed by a broad
    // hand-written rule like `web-fetch` (GET on *).
    let mut warnings: Vec<String> = Vec::new();
    let mut connections: Vec<Connection> = Vec::new();
    let mut connection_rules: Vec<Value> = Vec::new();
    let registry = crate::credential::registry::registry_json();
    let mut connection_files: Vec<PathBuf> = std::fs::read_dir(paths::connections_dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("toml"))
        .collect();
    connection_files.sort();
    for path in connection_files {
        let name = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let v = match read_toml(&path) {
            Ok(v) => v,
            Err(e) => {
                errors.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        match compile_connection(
            &name,
            &v,
            &workers,
            |p| registry.contains_key(p),
            |c| crate::credential::vault::get_credential(c).is_some(),
        ) {
            Ok((connection, rules, connection_exposes, warning)) => {
                connection_rules.extend(rules);
                exposes.extend(connection_exposes);
                warnings.extend(warning);
                connections.push(connection);
            }
            Err(mut e) => errors.append(&mut e),
        }
    }
    connection_rules.extend(rules);
    let rules = connection_rules;

    // Validate by deserializing into the runtime's own types.
    let policy = parse::<Policy>(&mut errors, "policy (grants)", json!({ "rules": rules }));
    let budget = parse::<BudgetPolicy>(
        &mut errors,
        "budget",
        json!({
            "scope": "org",
            "currencies": org_budget.and_then(|b| b.get("currencies")).map(to_json).unwrap_or(json!([])),
            "vars": org_budget.and_then(|b| b.get("vars")).map(to_json).unwrap_or(json!({})),
            "meters": org_budget.map(|b| array(b, "meter")).unwrap_or_default().iter().map(|m| to_json(m)).collect::<Vec<_>>(),
            "limits": limits,
        }),
    );
    let actions = parse::<ActionPolicy>(
        &mut errors,
        "actions/trust",
        json!({ "actions": actions, "trust": trust }),
    );

    validate_exposes(
        &exposes,
        |name| crate::credential::vault::get_credential(name).is_some(),
        &mut errors,
    );

    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(Loaded {
        policy,
        budget,
        actions,
        heartbeats,
        memory: CompiledMemoryPolicy {
            default: default_memory,
            workers: worker_memory,
        },
        context: CompiledContextPolicy {
            default: default_context,
            workers: worker_context,
        },
        storage: CompiledStoragePolicy {
            default: default_storage,
            workers: worker_storage,
        },
        listeners,
        exposes,
        connections,
        warnings,
        workers,
        engine_dir,
        box_policy,
    })
}

/// Compile one connection file into (record, judge rules, exposures, warning).
/// What one connection file compiles into: the record, the judge rules it
/// contributes, its env exposures, and a warning when it is disabled.
type CompiledConnection = (Connection, Vec<Value>, Vec<Expose>, Option<String>);

/// Pure over the injected lookups, so it is unit-testable.
fn compile_connection(
    name: &str,
    v: &toml::Value,
    known_workers: &[String],
    provider_exists: impl Fn(&str) -> bool,
    secret_exists: impl Fn(&str) -> bool,
) -> Result<CompiledConnection, Vec<String>> {
    let mut errors = Vec::new();
    let ctx = format!("connection \"{name}\"");

    let provider = v
        .get("provider")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    let inject_header = v
        .get("inject_header")
        .and_then(|x| x.as_str())
        .map(str::to_string);
    let inject_value = v
        .get("inject_value")
        .and_then(|x| x.as_str())
        .map(str::to_string);
    let inline_inject = match (&inject_header, &inject_value) {
        (Some(_), Some(_)) => true,
        (None, None) => false,
        _ => {
            errors.push(format!(
                "{ctx}: inject_header and inject_value must be provided together"
            ));
            false
        }
    };
    if provider.is_empty() {
        errors.push(format!("{ctx}: needs provider = \"<registry name>\""));
    } else if !provider_exists(&provider) && !inline_inject {
        errors.push(format!(
            "{ctx}: unknown provider \"{provider}\" (add inject_header + inject_value or declare it in providers.toml)"
        ));
    }

    let strings = |key: &str| -> Option<Vec<String>> {
        v.get(key).and_then(|x| x.as_array()).map(|a| {
            a.iter()
                .filter_map(|s| s.as_str())
                .map(str::to_string)
                .collect()
        })
    };
    let org_scoped = v.get("scope").and_then(|x| x.as_str()) == Some("org");
    // Accept the pre-rename key so upgraded deployments keep parsing.
    let workers = strings("workers").or_else(|| strings("imps"));
    match (&workers, org_scoped) {
        (Some(_), true) => errors.push(format!(
            "{ctx}: choose workers = [..] OR scope = \"org\", not both"
        )),
        (None, false) => errors.push(format!(
            "{ctx}: needs workers = [\"<name>\", ..] or scope = \"org\""
        )),
        (Some(list), false) => {
            for w in list {
                if !known_workers.contains(w) {
                    errors.push(format!("{ctx}: no such worker \"{w}\""));
                }
            }
            if list.is_empty() {
                errors.push(format!(
                    "{ctx}: workers = [] grants nothing — use scope = \"org\" or name workers"
                ));
            }
        }
        (None, true) => {}
    }

    let hosts = strings("hosts").unwrap_or_default();
    if hosts.is_empty() {
        errors.push(format!("{ctx}: needs hosts = [\"api.example.com\", ..]"));
    }
    let methods = strings("methods").unwrap_or_else(|| vec!["GET".into()]);
    let env = v
        .get("env")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    if env.is_empty() {
        errors.push(format!("{ctx}: needs env = \"<VAR the box sees>\""));
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    let enabled = secret_exists(name);
    let connection = Connection {
        name: name.to_string(),
        provider: provider.clone(),
        workers: workers.clone(),
        hosts: hosts.clone(),
        methods: methods.clone(),
        env: env.clone(),
        enabled,
    };
    if !enabled {
        // Disabled, not broken: no grant, no exposure, nothing to inject —
        // and the rest of the config keeps working.
        let fix = if name == connection.provider {
            format!("roster connection add {name}")
        } else {
            format!(
                "roster connection add {} --name {name}",
                connection.provider
            )
        };
        let warning =
            format!("{ctx} is disabled — no \"{name}\" credential in the vault (run: {fix})");
        return Ok((connection, Vec::new(), Vec::new(), Some(warning)));
    }

    let scopes: Vec<String> = match &workers {
        Some(list) => list.iter().map(|w| format!("org/{w}")).collect(),
        None => vec!["org".to_string()],
    };
    let rules = scopes
        .iter()
        .map(|scope| {
            let mut inject = json!({ "credential": name, "provider": provider });
            if let (Some(header), Some(value)) = (&inject_header, &inject_value) {
                inject["headers"] = json!([{ "header": header, "value": value }]);
            }
            json!({
                "scope": scope,
                "name": format!("connection:{name}"),
                "match": { "host": hosts, "port": 443, "method": methods },
                "verdict": "allow",
                "inject": inject,
            })
        })
        .collect();
    let exposes = scopes
        .into_iter()
        .map(|scope| Expose {
            scope,
            credential: name.to_string(),
            env: env.clone(),
        })
        .collect();
    Ok((connection, rules, exposes, None))
}

/// The env vars provisioning owns — an `[[expose]]` may not overwrite the
/// box's wiring (proxy, trust, identity), only add credential placeholders.
const RESERVED_ENV: &[&str] = &[
    "HOME",
    "TMPDIR",
    "PI_CODING_AGENT_DIR",
    "ANTHROPIC_API_KEY",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "NODE_USE_ENV_PROXY",
    "NODE_EXTRA_CA_CERTS",
    "SSL_CERT_FILE",
    "CURL_CA_BUNDLE",
    "REQUESTS_CA_BUNDLE",
    "GIT_SSL_CAINFO",
    "PIP_CERT",
];

fn parse_expose(
    v: &toml::Value,
    scope: &str,
    source: &str,
    exposes: &mut Vec<Expose>,
    errors: &mut Vec<String>,
) {
    let field = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    match (field("credential"), field("env")) {
        (Some(credential), Some(env)) => exposes.push(Expose {
            scope: scope.to_string(),
            credential,
            env,
        }),
        _ => errors.push(format!(
            "{source} [[expose]]: needs string fields \"credential\" and \"env\""
        )),
    }
}

/// Every exposure must name a real credential (fail closed, like listener
/// credentials), a well-formed env name outside the reserved wiring, and no
/// two exposures that could reach the same worker may claim one env name.
fn validate_exposes(
    exposes: &[Expose],
    credential_exists: impl Fn(&str) -> bool,
    errors: &mut Vec<String>,
) {
    for (i, e) in exposes.iter().enumerate() {
        let well_formed = !e.env.is_empty()
            && !e.env.as_bytes()[0].is_ascii_digit()
            && e.env
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
        if !well_formed {
            errors.push(format!(
                "[[expose]] env \"{}\": use UPPER_SNAKE_CASE",
                e.env
            ));
        }
        if RESERVED_ENV.contains(&e.env.as_str()) || e.env.starts_with("ROSTER_") {
            errors.push(format!(
                "[[expose]] env \"{}\" is reserved box wiring",
                e.env
            ));
        }
        if !credential_exists(&e.credential) {
            errors.push(format!(
                "[[expose]] {}: no \"{}\" credential in the vault — run: roster connection add <provider>",
                e.env, e.credential
            ));
        }
        for other in &exposes[i + 1..] {
            let overlap = e.scope == other.scope || e.scope == "org" || other.scope == "org";
            if e.env == other.env && overlap {
                errors.push(format!(
                    "[[expose]] env \"{}\" claimed twice for overlapping scopes {} and {}",
                    e.env, e.scope, other.scope
                ));
            }
        }
    }
}

/// The cached view. Reloads when any config file's fingerprint changes, so
/// admin edits are live without a restart. On invalid config returns Err —
/// callers fail closed.
pub fn snapshot() -> Result<Arc<Loaded>, String> {
    /// The cached config, keyed by the fingerprint it was loaded from.
    type Cached = Mutex<Option<(String, Arc<Loaded>)>>;
    static CACHE: OnceLock<Cached> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let fp = fingerprint();
    {
        let cached = cache.lock().unwrap();
        if let Some((cached_fp, loaded)) = cached.as_ref() {
            if *cached_fp == fp {
                return Ok(loaded.clone());
            }
        }
    }
    match load() {
        Ok(loaded) => {
            let loaded = Arc::new(loaded);
            *cache.lock().unwrap() = Some((fp, loaded.clone()));
            Ok(loaded)
        }
        Err(errors) => Err(errors.join("\n")),
    }
}

/// mtime+len of every config file, so an edit anywhere invalidates the cache.
fn fingerprint() -> String {
    fn stamp(path: &std::path::Path) -> String {
        std::fs::metadata(path)
            .map(|m| {
                let mtime = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                format!("{}:{mtime}:{}", path.display(), m.len())
            })
            .unwrap_or_else(|_| format!("{}:absent", path.display()))
    }
    let mut parts = vec![stamp(&paths::org_file())];
    let mut names: Vec<PathBuf> = std::fs::read_dir(paths::workers_dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join("worker.toml"))
        .collect();
    names.sort();
    for spec in names {
        parts.push(stamp(&spec));
    }
    let mut connections: Vec<PathBuf> = std::fs::read_dir(paths::connections_dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    connections.sort();
    for c in connections {
        parts.push(stamp(&c));
    }
    // A connection's enabled-ness lives in the vault: the DIR mtime moves on
    // credential create/delete (not on token refresh rewrites, which must not
    // thrash this cache).
    parts.push(stamp(&crate::paths::vault_dir()));
    parts.join("|")
}

// ── helpers (moved from the retired deploy step) ─────────────────────────────

fn parse<T: serde::de::DeserializeOwned + Default>(
    errors: &mut Vec<String>,
    what: &str,
    v: Value,
) -> T {
    match serde_json::from_value::<T>(v) {
        Ok(t) => t,
        Err(e) => {
            errors.push(format!("{what}: {e}"));
            T::default()
        }
    }
}

type BErr = Box<dyn std::error::Error>;

fn context_policy(
    value: Option<&toml::Value>,
    base: Option<&ContextPolicy>,
) -> Result<ContextPolicy, BErr> {
    let mut merged = serde_json::to_value(base.cloned().unwrap_or_default())?;
    if let Some(value) = value {
        merge_json(&mut merged, to_json(value));
    }
    serde_json::from_value(merged).map_err(|e| format!("context policy is invalid: {e}").into())
}

fn memory_policy(
    value: Option<&toml::Value>,
    base: Option<&MemoryPolicy>,
) -> Result<MemoryPolicy, BErr> {
    let mut merged = serde_json::to_value(base.cloned().unwrap_or_default())?;
    if let Some(value) = value {
        merge_json(&mut merged, to_json(value));
    }
    let policy: MemoryPolicy =
        serde_json::from_value(merged).map_err(|e| format!("memory policy is invalid: {e}"))?;
    if let Some(kind) = policy
        .allowed_kinds
        .iter()
        .find(|kind| !crate::worker::memory::SUPPORTED_MEMORY_KINDS.contains(&kind.as_str()))
    {
        return Err(format!(
            "memory policy kind \"{kind}\" is not interaction memory; supported kinds are {}",
            crate::worker::memory::SUPPORTED_MEMORY_KINDS.join(", ")
        )
        .into());
    }
    Ok(policy)
}

fn storage_policy(
    value: &toml::Value,
    base: Option<&StoragePolicy>,
) -> Result<StoragePolicy, BErr> {
    let mut merged = serde_json::to_value(base.cloned().unwrap_or_default())?;
    let overlay = json!({
        "knowledge": value.get("knowledge").map(to_json).unwrap_or(json!({})),
    });
    merge_json(&mut merged, overlay);
    let policy: StoragePolicy = serde_json::from_value(merged)
        .map_err(|error| format!("storage policy is invalid: {error}"))?;
    crate::worker::storage::validate(&policy)
        .map_err(|error| format!("storage policy is invalid: {error}"))?;
    Ok(policy)
}

fn merge_json(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(existing) => merge_json(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

fn read_toml(path: &std::path::Path) -> Result<toml::Value, BErr> {
    if !path.exists() {
        return Ok(toml::Value::Table(Default::default()));
    }
    Ok(toml::from_str(&std::fs::read_to_string(path)?)?)
}

/// The array of tables under `key` in a TOML table (`[[key]]`), or empty.
fn array<'a>(v: &'a toml::Value, key: &str) -> Vec<&'a toml::Value> {
    v.get(key)
        .and_then(|x| x.as_array())
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

fn to_json(v: &toml::Value) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

fn with_scope(v: &toml::Value, scope: &str) -> Value {
    let mut j = to_json(v);
    if let Some(obj) = j.as_object_mut() {
        obj.insert("scope".to_string(), json!(scope));
    }
    j
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expose(scope: &str, credential: &str, env: &str) -> Expose {
        Expose {
            scope: scope.into(),
            credential: credential.into(),
            env: env.into(),
        }
    }

    fn toml(s: &str) -> toml::Value {
        toml::from_str(s).unwrap()
    }

    #[test]
    fn connection_compiles_per_worker_grants_and_exposes() {
        let v = toml(
            r#"
            provider = "github"
            workers = ["yuko", "kdemo"]
            hosts = ["api.github.com"]
            env = "GH_TOKEN"
        "#,
        );
        let workers = vec!["yuko".to_string(), "kdemo".to_string()];
        let (c, rules, exposes, warning) =
            compile_connection("github", &v, &workers, |_| true, |_| true).unwrap();
        assert!(c.enabled);
        assert_eq!(c.methods, vec!["GET"]); // the default
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0]["scope"], "org/yuko");
        assert_eq!(rules[0]["name"], "connection:github");
        assert_eq!(rules[0]["match"]["host"][0], "api.github.com");
        assert_eq!(rules[0]["inject"]["credential"], "github");
        assert_eq!(exposes.len(), 2);
        assert_eq!(exposes[1].scope, "org/kdemo");
        assert_eq!(exposes[1].env, "GH_TOKEN");
        assert!(warning.is_none());
    }

    #[test]
    fn generic_connection_carries_its_inline_injection_template() {
        let v = toml(
            r#"
            provider = "acme"
            scope = "org"
            hosts = ["api.acme.test"]
            methods = ["GET", "POST"]
            env = "ACME_TOKEN"
            inject_header = "authorization"
            inject_value = "Bearer {key}"
        "#,
        );
        let (_, rules, _, _) = compile_connection("acme", &v, &[], |_| false, |_| true).unwrap();
        assert_eq!(rules[0]["inject"]["provider"], "acme");
        assert_eq!(rules[0]["inject"]["headers"][0]["value"], "Bearer {key}");
    }

    #[test]
    fn connection_without_secret_is_disabled_not_broken() {
        let v = toml(
            r#"
            provider = "github"
            scope = "org"
            hosts = ["api.github.com"]
            env = "GH_TOKEN"
        "#,
        );
        let (c, rules, exposes, warning) =
            compile_connection("github", &v, &[], |_| true, |_| false).unwrap();
        assert!(!c.enabled);
        assert!(rules.is_empty() && exposes.is_empty());
        assert!(warning.unwrap().contains("disabled"));
    }

    #[test]
    fn connection_validation_catches_each_failure_mode() {
        let v = toml(
            r#"
            provider = "nope"
            workers = ["ghost"]
            env = ""
        "#,
        );
        let errors = compile_connection(
            "acme",
            &v,
            &["yuko".to_string()],
            |p| p == "github",
            |_| true,
        )
        .unwrap_err();
        assert!(errors.iter().any(|e| e.contains("unknown provider")));
        assert!(errors
            .iter()
            .any(|e| e.contains("no such worker \"ghost\"")));
        assert!(errors.iter().any(|e| e.contains("needs hosts")));
        assert!(errors.iter().any(|e| e.contains("needs env")));
    }

    #[test]
    fn expose_validation_catches_each_failure_mode() {
        let vault = |name: &str| name == "github";

        // Well-formed, distinct workers sharing an env name: fine.
        let mut errors = Vec::new();
        let ok = [
            expose("org/a", "github", "GH_TOKEN"),
            expose("org/b", "github", "GH_TOKEN"),
        ];
        validate_exposes(&ok, vault, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");

        // Reserved wiring, bad shape, unknown credential, org-scope duplicate.
        let mut errors = Vec::new();
        let bad = [
            expose("org", "github", "HTTP_PROXY"),
            expose("org", "github", "ROSTER_X"),
            expose("org", "github", "lower"),
            expose("org", "nope", "A_TOKEN"),
            expose("org", "github", "B_TOKEN"),
            expose("org/a", "github", "B_TOKEN"),
        ];
        validate_exposes(&bad, vault, &mut errors);
        assert_eq!(errors.len(), 5, "{errors:?}");
        assert!(errors
            .iter()
            .any(|e| e.contains("reserved") && e.contains("HTTP_PROXY")));
        assert!(errors
            .iter()
            .any(|e| e.contains("reserved") && e.contains("ROSTER_X")));
        assert!(errors.iter().any(|e| e.contains("UPPER_SNAKE_CASE")));
        assert!(errors.iter().any(|e| e.contains("no \"nope\" credential")));
        assert!(errors.iter().any(|e| e.contains("claimed twice")));
    }
}
