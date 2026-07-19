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
use crate::worker::storage::{CompiledStoragePolicy, StoragePolicy};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

/// The published box image. Tracks `:latest` deliberately: the host re-pulls
/// on every server start, so deployments stay current without a binary
/// upgrade. `[engine] image` in org.toml overrides (e.g. a local build).
pub const DEFAULT_BOX_IMAGE: &str = "ghcr.io/manasgarg/roster-box:latest";

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
    /// `[restrict]` — provider-declared scope dimensions narrowing the grant
    /// (registry `scope_dims`; discord: servers/channels). One scope, every
    /// enforcement point: listeners refuse attachment outside it, the gateway
    /// compiles it into path predicates. Empty = unrestricted.
    pub restrict: std::collections::BTreeMap<String, Vec<String>>,
}

impl Connection {
    /// The channel/server restriction a listener must enforce, if any.
    /// None = unrestricted. Ids are Discord snowflakes as strings.
    pub fn allows_surface(&self, server_id: Option<&str>, channel_id: &str) -> bool {
        let servers = self.restrict.get("servers");
        let channels = self.restrict.get("channels");
        if servers.is_none() && channels.is_none() {
            return true;
        }
        // A surface is in scope if EITHER dimension admits it: a listed
        // channel is reachable even when its server isn't listed, and a
        // listed server admits all its channels.
        if let Some(list) = channels {
            if list.iter().any(|c| c == channel_id) {
                return true;
            }
        }
        if let (Some(list), Some(sid)) = (servers, server_id) {
            if list.iter().any(|s| s == sid) {
                return true;
            }
        }
        false
    }
}

/// A host resource connection (`kind = "host-dir"` / `"host-repo"` in
/// `connections/<name>.toml`): no secret, no gateway rules — granting one
/// materializes it in the box filesystem under `$HOME/mnt/<name>`
/// (docs/plans/worker-environment.md). The name doubles as the mount
/// directory, so it is restricted to path-safe characters.
#[derive(Clone, Debug)]
pub struct HostMount {
    pub name: String,
    pub kind: HostMountKind,
    pub path: PathBuf,
    /// None = org-wide; Some = these workers only.
    pub workers: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostMountKind {
    /// `kind = "host-dir"` — a plain directory, `mode = "ro"` (default) or
    /// `"rw"`. An rw grant on a dir roster doesn't back up warns at load:
    /// no gate, no snapshots — a bad run's writes there are unrecoverable.
    Dir { rw: bool },
    /// `kind = "host-repo"` — a git repository, `write = "ro"` (default) or
    /// `"gated"`: the run works on a branch and lands it through the
    /// validated `repo_push` action; the host stays sole writer of `branch`.
    Repo { gated: bool, branch: String },
}

impl HostMount {
    pub fn applies_to(&self, worker: &str) -> bool {
        match &self.workers {
            None => true,
            Some(list) => list.iter().any(|w| w == worker),
        }
    }
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
    /// Host-dir / host-repo connections — materialized as box mounts at
    /// provision time, never as gateway rules.
    pub host_mounts: Vec<HostMount>,
    /// Non-fatal conditions (e.g. a disabled connection) — printed by
    /// `validate` and `server start`, never fail-closed.
    pub warnings: Vec<String>,
    pub workers: Vec<String>,
    /// `[engine] dir` in org.toml — a dev checkout mounted read-only over the
    /// engine baked into the box image. Unset (the default) runs the
    /// baked engine.
    pub engine_dir: Option<PathBuf>,
    /// The box image workers run in — `[engine] image` in org.toml, or the
    /// published image (always `:latest`; the host re-pulls at server start).
    /// Point it at a locally built tag to iterate on the Dockerfile.
    pub box_image: String,
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

    let default_context = context_policy(org.get("context"), None).unwrap_or_else(|e| {
        errors.push(format!("org.toml [context]: {e}"));
        ContextPolicy::default()
    });
    let default_storage = storage_policy(&org, None).unwrap_or_else(|e| {
        errors.push(format!("org.toml [knowledge]: {e}"));
        StoragePolicy::default()
    });
    let mut worker_context = std::collections::HashMap::new();
    let mut worker_storage = std::collections::HashMap::new();

    warn_rule_shape(&org, "org.toml", &mut errors);
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

    let box_image = match org.get("engine").and_then(|e| e.get("image")) {
        None => DEFAULT_BOX_IMAGE.to_string(),
        Some(v) => match v.as_str().map(str::trim) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                errors.push(
                    "org.toml [engine] image: must be a non-empty string — an image \
                     reference (registry or locally built tag)"
                        .into(),
                );
                DEFAULT_BOX_IMAGE.to_string()
            }
        },
    };

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
            warn_rule_shape(&w, &name, &mut errors);
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
            let heartbeat = match w.get("heartbeat") {
                None => "every 30m".to_string(),
                Some(v) => match v.as_str() {
                    Some(s) => s.to_string(),
                    // Present but wrong-typed (e.g. `heartbeat = 60`) — don't
                    // silently substitute the default; say so.
                    None => {
                        errors.push(format!(
                            "{name}: heartbeat must be a string like \"30m\" or \"off\", not {v}"
                        ));
                        "every 30m".to_string()
                    }
                },
            };
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
    let mut host_mounts: Vec<HostMount> = Vec::new();
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
        let kind = v.get("kind").and_then(|x| x.as_str()).unwrap_or("service");
        match kind {
            "service" => {}
            "host-dir" | "host-repo" => {
                match compile_host_mount(&name, kind, &v, &workers) {
                    Ok((mount, mut warns)) => {
                        warnings.append(&mut warns);
                        host_mounts.push(mount);
                    }
                    Err(mut e) => errors.append(&mut e),
                }
                continue;
            }
            other => {
                errors.push(format!(
                    "connection \"{name}\": unknown kind \"{other}\" (service, host-dir, host-repo)"
                ));
                continue;
            }
        }
        match compile_connection(
            &name,
            &v,
            &workers,
            |p| registry.contains_key(p),
            |c| crate::credential::vault::get_credential(c).is_some(),
            |p| {
                registry.get(p).and_then(|entry| {
                    entry.get("model_hosts").and_then(Value::as_array).map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                })
            },
            |p| {
                registry
                    .get(p)
                    .and_then(|entry| entry.get("scope_dims").and_then(Value::as_array))
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default()
            },
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
        host_mounts,
        warnings,
        workers,
        engine_dir,
        box_image,
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
    model_hosts_of: impl Fn(&str) -> Option<Vec<String>>,
    scope_dims_of: impl Fn(&str) -> Vec<String>,
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

    // A model provider's connection is a grant by default: hosts come from
    // the registry's model_hosts and there is no env exposure — the box
    // authenticates through sentinel logins and the gateway injects the real
    // credential in transit.
    let model_hosts = model_hosts_of(&provider);
    let is_model = model_hosts.is_some();
    let hosts = strings("hosts").or(model_hosts).unwrap_or_default();
    if hosts.is_empty() {
        errors.push(format!("{ctx}: needs hosts = [\"api.example.com\", ..]"));
    }
    // No methods key = no method limit ("*"): a connection grants the service,
    // not a verb subset. A methods = [..] line in the file narrows it.
    let methods = strings("methods").unwrap_or_else(|| vec!["*".into()]);
    let env = v
        .get("env")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    if env.is_empty() && !is_model {
        errors.push(format!("{ctx}: needs env = \"<VAR the box sees>\""));
    }

    // `[restrict]` — one scope declaration, validated against the provider's
    // registry-declared dimensions. Unknown dimension = config error, not a
    // silently ignored key: a restriction the operator believes exists MUST
    // exist.
    let mut restrict: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    if let Some(r) = v.get("restrict") {
        match r.as_table() {
            Some(table) => {
                let dims = scope_dims_of(&provider);
                for (dim, val) in table {
                    if !dims.iter().any(|d| d == dim) {
                        let declared = if dims.is_empty() {
                            "it declares none".to_string()
                        } else {
                            format!("it declares: {}", dims.join(", "))
                        };
                        errors.push(format!(
                            "{ctx}: provider \"{provider}\" has no scope dimension \"{dim}\" ({declared})"
                        ));
                        continue;
                    }
                    let ids: Vec<String> = val
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|s| s.as_str())
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    if ids.is_empty() {
                        errors.push(format!(
                            "{ctx}: restrict.{dim} needs a non-empty list of id strings"
                        ));
                        continue;
                    }
                    restrict.insert(dim.clone(), ids);
                }
            }
            None => errors.push(format!(
                "{ctx}: [restrict] must be a table of <dimension> = [\"id\", ..]"
            )),
        }
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
        restrict: restrict.clone(),
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
    let mut rules: Vec<Value> = Vec::new();
    for scope in &scopes {
        let mut inject = json!({ "credential": name, "provider": provider });
        if let (Some(header), Some(value)) = (&inject_header, &inject_value) {
            inject["headers"] = json!([{ "header": header, "value": value }]);
        }
        // A restricted discord connection compiles its scope into path
        // predicates, first-match-wins: allow the scoped surfaces, deny the
        // rest of that resource family, then the broad host allow for
        // everything else the API needs (users/@me, the gateway URL, …).
        //
        // Known limit, by design: a servers-only restriction can't be fully
        // enforced on `/channels/<id>` paths — Discord channel endpoints
        // don't carry the guild id — so there the listener's attachment rule
        // is the enforcement and the gateway stays broad. A channels
        // restriction IS fully enforced here.
        if provider == "discord" && !restrict.is_empty() {
            for id in restrict.get("channels").into_iter().flatten() {
                rules.push(json!({
                    "scope": scope,
                    "name": format!("connection:{name}:channel:{id}"),
                    "match": { "host": hosts, "port": 443, "method": methods,
                               "pathPrefix": format!("/api/v10/channels/{id}") },
                    "verdict": "allow",
                    "inject": inject,
                }));
            }
            for id in restrict.get("servers").into_iter().flatten() {
                rules.push(json!({
                    "scope": scope,
                    "name": format!("connection:{name}:server:{id}"),
                    "match": { "host": hosts, "port": 443, "method": methods,
                               "pathPrefix": format!("/api/v10/guilds/{id}") },
                    "verdict": "allow",
                    "inject": inject,
                }));
            }
            if restrict.contains_key("channels") && !restrict.contains_key("servers") {
                rules.push(json!({
                    "scope": scope,
                    "name": format!("connection:{name}:deny-unscoped-channels"),
                    "match": { "host": hosts, "port": 443,
                               "pathPrefix": "/api/v10/channels" },
                    "verdict": "deny",
                }));
            }
            rules.push(json!({
                "scope": scope,
                "name": format!("connection:{name}:deny-unscoped-servers"),
                "match": { "host": hosts, "port": 443,
                           "pathPrefix": "/api/v10/guilds" },
                "verdict": "deny",
            }));
        }
        rules.push(json!({
            "scope": scope,
            "name": format!("connection:{name}"),
            "match": { "host": hosts, "port": 443, "method": methods },
            "verdict": "allow",
            "inject": inject,
        }));
    }
    let exposes = if env.is_empty() {
        Vec::new()
    } else {
        scopes
            .into_iter()
            .map(|scope| Expose {
                scope,
                credential: name.to_string(),
                env: env.clone(),
            })
            .collect()
    };
    Ok((connection, rules, exposes, None))
}

/// Compile a `kind = "host-dir"` / `"host-repo"` connection file. No secret,
/// no rules — validation is about the path and the grant. Fail closed on a
/// missing path: a mount that silently doesn't appear is worse than a boot
/// refusal, because the worker was promised the resource.
fn compile_host_mount(
    name: &str,
    kind: &str,
    v: &toml::Value,
    known_workers: &[String],
) -> Result<(HostMount, Vec<String>), Vec<String>> {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let ctx = format!("connection \"{name}\"");

    // The name becomes the container path `mnt/<name>` — path-safe only.
    let name_ok = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if !name_ok {
        errors.push(format!(
            "{ctx}: host mount names must be lowercase [a-z0-9-_] (the name is the mount directory)"
        ));
    }

    let path = v
        .get("path")
        .and_then(|x| x.as_str())
        .map(PathBuf::from)
        .unwrap_or_default();
    if !path.is_absolute() {
        errors.push(format!("{ctx}: needs an absolute path = \"/…\""));
    } else if !path.is_dir() {
        errors.push(format!("{ctx}: path {} is not a directory", path.display()));
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
    let workers = strings("workers");
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

    let mount_kind = match kind {
        "host-dir" => {
            let mode = v.get("mode").and_then(|x| x.as_str()).unwrap_or("ro");
            match mode {
                "ro" => HostMountKind::Dir { rw: false },
                "rw" => {
                    warnings.push(format!(
                        "{ctx}: rw grant on a dir roster does not back up — no gate, no \
                         snapshots; a bad run's writes there are unrecoverable by roster"
                    ));
                    HostMountKind::Dir { rw: true }
                }
                other => {
                    errors.push(format!("{ctx}: mode must be \"ro\" or \"rw\", not \"{other}\""));
                    HostMountKind::Dir { rw: false }
                }
            }
        }
        "host-repo" => {
            let write = v.get("write").and_then(|x| x.as_str()).unwrap_or("ro");
            let gated = match write {
                "ro" => false,
                "gated" => true,
                other => {
                    errors.push(format!(
                        "{ctx}: write must be \"ro\" or \"gated\", not \"{other}\""
                    ));
                    false
                }
            };
            // Bare repo (HEAD at the root) or a checkout (.git inside).
            if path.is_dir() && !path.join("HEAD").is_file() && !path.join(".git").exists() {
                errors.push(format!(
                    "{ctx}: path {} is not a git repository",
                    path.display()
                ));
            }
            // A gated repo's main is advanced by update-ref; on a checkout
            // that desyncs the worktree the operator is looking at. Bare only.
            if gated && path.is_dir() && !path.join("HEAD").is_file() {
                errors.push(format!(
                    "{ctx}: write = \"gated\" needs a bare repository (got a checkout — \
                     make one: git clone --bare)"
                ));
            }
            let branch = v
                .get("branch")
                .and_then(|x| x.as_str())
                .unwrap_or("main")
                .to_string();
            HostMountKind::Repo { gated, branch }
        }
        _ => unreachable!("caller routes only host kinds here"),
    };

    if !errors.is_empty() {
        return Err(errors);
    }
    Ok((
        HostMount {
            name: name.to_string(),
            kind: mount_kind,
            path,
            workers,
        },
        warnings,
    ))
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
    // providers.toml feeds registry_json(), which load() validates connections
    // against — so an edit here must invalidate the cache too, or the daemon
    // serves a policy that `validate` already rejects.
    let mut parts = vec![stamp(&paths::org_file()), stamp(&paths::providers_file())];
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


fn storage_policy(
    value: &toml::Value,
    base: Option<&StoragePolicy>,
) -> Result<StoragePolicy, BErr> {
    let mut merged = serde_json::to_value(base.cloned().unwrap_or_default())?;
    let overlay = json!({
        "knowledge": value.get("knowledge").map(to_json).unwrap_or(json!({})),
        "store": value.get("store").map(to_json).unwrap_or(json!({})),
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

/// Flag the common `[grant]` (single table) written where `[[grant]]` (array of
/// tables) was meant: array() silently ignores the former, so the rule just
/// never exists and `validate` would otherwise call the config good.
fn warn_rule_shape(v: &toml::Value, ctx: &str, errors: &mut Vec<String>) {
    for key in ["grant", "action", "trust", "expose"] {
        if v.get(key).map(|x| !x.is_array()).unwrap_or(false) {
            errors.push(format!(
                "{ctx}: [{key}] must be a table array — write [[{key}]] (double brackets), not [{key}]"
            ));
        }
    }
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
            compile_connection("github", &v, &workers, |_| true, |_| true, |_| None, |_| Vec::new()).unwrap();
        assert!(c.enabled);
        assert_eq!(c.methods, vec!["*"]); // the default: full access
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
        let (_, rules, _, _) =
            compile_connection("acme", &v, &[], |_| false, |_| true, |_| None, |_| Vec::new()).unwrap();
        assert_eq!(rules[0]["inject"]["provider"], "acme");
        assert_eq!(rules[0]["inject"]["headers"][0]["value"], "Bearer {key}");
    }

    #[test]
    fn model_connection_is_a_grant_by_default() {
        // Two lines of toml — hosts, methods, and the no-exposure shape all
        // derive from the provider being a model (registry model_hosts).
        let v = toml(
            r#"
            provider = "anthropic"
            scope = "org"
        "#,
        );
        let (c, rules, exposes, warning) = compile_connection(
            "anthropic",
            &v,
            &[],
            |_| true,
            |_| true,
            |p| (p == "anthropic").then(|| vec!["api.anthropic.com".to_string()]),
            |_| Vec::new(),
        )
        .unwrap();
        assert!(c.enabled);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0]["scope"], "org");
        assert_eq!(rules[0]["match"]["host"][0], "api.anthropic.com");
        assert_eq!(rules[0]["match"]["method"][0], "*");
        assert_eq!(rules[0]["inject"]["credential"], "anthropic");
        assert!(exposes.is_empty(), "models expose no env var");
        assert!(warning.is_none());
    }

    #[test]
    fn restricted_discord_connection_compiles_scoped_rules() {
        let v = toml(
            r#"
            provider = "discord"
            workers = ["yuko"]
            hosts = ["discord.com"]
            env = "DISCORD_TOKEN"
            [restrict]
            channels = ["111", "222"]
        "#,
        );
        let dims = |p: &str| {
            if p == "discord" {
                vec!["servers".to_string(), "channels".to_string()]
            } else {
                Vec::new()
            }
        };
        let (c, rules, _, _) = compile_connection(
            "discord",
            &v,
            &["yuko".to_string()],
            |_| true,
            |_| true,
            |_| None,
            dims,
        )
        .unwrap();
        assert!(c.allows_surface(None, "111"));
        assert!(!c.allows_surface(None, "333"));
        // allow 111, allow 222, deny unscoped channels, deny guilds, broad allow
        assert_eq!(rules.len(), 5);
        assert_eq!(
            rules[0]["match"]["pathPrefix"],
            "/api/v10/channels/111"
        );
        assert_eq!(rules[2]["verdict"], "deny");
        assert_eq!(rules[4]["name"], "connection:discord");
        assert!(rules[4]["match"]["pathPrefix"].is_null());

        // A server restriction admits the whole guild: no channels deny.
        let v = toml(
            r#"
            provider = "discord"
            workers = ["yuko"]
            hosts = ["discord.com"]
            env = "DISCORD_TOKEN"
            [restrict]
            servers = ["999"]
        "#,
        );
        let (c, rules, _, _) = compile_connection(
            "discord",
            &v,
            &["yuko".to_string()],
            |_| true,
            |_| true,
            |_| None,
            dims,
        )
        .unwrap();
        assert!(c.allows_surface(Some("999"), "any-channel"));
        assert!(!c.allows_surface(Some("998"), "any-channel"));
        assert!(rules
            .iter()
            .all(|r| r["name"] != "connection:discord:deny-unscoped-channels"));
    }

    #[test]
    fn restrict_on_undeclared_dimension_is_an_error() {
        let v = toml(
            r#"
            provider = "github"
            scope = "org"
            hosts = ["api.github.com"]
            env = "GH_TOKEN"
            [restrict]
            channels = ["111"]
        "#,
        );
        let errors = compile_connection(
            "github",
            &v,
            &[],
            |_| true,
            |_| true,
            |_| None,
            |_| Vec::new(),
        )
        .unwrap_err();
        assert!(errors.iter().any(|e| e.contains("no scope dimension")));
    }

    #[test]
    fn host_dir_mount_parses_and_rw_warns() {
        let dir = tempfile::tempdir().unwrap();
        let v = toml(&format!(
            r#"
            kind = "host-dir"
            path = "{}"
            mode = "rw"
            workers = ["yuko"]
        "#,
            dir.path().display()
        ));
        let (m, warns) =
            compile_host_mount("notes", "host-dir", &v, &["yuko".to_string()]).unwrap();
        assert_eq!(m.kind, HostMountKind::Dir { rw: true });
        assert!(m.applies_to("yuko") && !m.applies_to("kdemo"));
        assert!(warns[0].contains("does not back up"));

        // ro is the default and warns about nothing
        let v = toml(&format!(
            r#"
            kind = "host-dir"
            path = "{}"
            scope = "org"
        "#,
            dir.path().display()
        ));
        let (m, warns) = compile_host_mount("notes", "host-dir", &v, &[]).unwrap();
        assert_eq!(m.kind, HostMountKind::Dir { rw: false });
        assert!(warns.is_empty());
    }

    #[test]
    fn host_repo_mount_requires_a_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        let v = toml(&format!(
            r#"
            kind = "host-repo"
            path = "{}"
            write = "gated"
            scope = "org"
        "#,
            dir.path().display()
        ));
        let errors = compile_host_mount("proj", "host-repo", &v, &[]).unwrap_err();
        assert!(errors.iter().any(|e| e.contains("not a git repository")));

        std::fs::write(dir.path().join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let (m, _) = compile_host_mount("proj", "host-repo", &v, &[]).unwrap();
        assert_eq!(
            m.kind,
            HostMountKind::Repo { gated: true, branch: "main".into() }
        );
    }

    #[test]
    fn host_mount_names_must_be_path_safe() {
        let dir = tempfile::tempdir().unwrap();
        let v = toml(&format!(
            r#"
            kind = "host-dir"
            path = "{}"
            scope = "org"
        "#,
            dir.path().display()
        ));
        let errors = compile_host_mount("Bad Name", "host-dir", &v, &[]).unwrap_err();
        assert!(errors.iter().any(|e| e.contains("path-safe")
            || e.contains("lowercase")));
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
            compile_connection("github", &v, &[], |_| true, |_| false, |_| None, |_| Vec::new()).unwrap();
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
            |_| None,
            |_| Vec::new(),
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
