//! Git-backed world knowledge for a worker — branch-per-run, host-owned main
//! (docs/plans/knowledge-branch-per-run.md). Every writable run works in a
//! real clone on its own branch and lands changes through the gated
//! `knowledge_push` action; the trusted host validates the pushed range and
//! advances `main` fast-forward only. A box can never write a canonical ref,
//! so `git log main` stays the audit trail. Divergence is always resolved in
//! the box (fetch/rebase/push again) — the host never merges content.

use crate::paths;
use crate::run::runlog::KnowledgeRunRecord;
use crate::worker::storage::KnowledgePolicy;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

/// Gated repos mount under the granted-connection tree: the run's clone at
/// `mnt/<connection>`, the canonical repo read-only at
/// `mnt/.origins/<connection>` as `origin` — so `git fetch origin` after a
/// stale push sees the new main immediately, while a ref write from the box
/// is a filesystem error, not a policy hope.
const MNT_BASE: &str = "/pihome/mnt";
/// Where the box's push tool writes the bundle: inside the clone's own .git,
/// so the worktree stays clean. The host derives this path from the run id —
/// never from box-supplied input.
const PUSH_BUNDLE: &str = "roster-push.bundle";
const ALLOWED_EXTENSIONS: &[&str] = &["md", "json", "jsonl", "yaml", "yml", "csv", "txt"];
/// Parked (backstopped) run branches expire after this many days.
const QUARANTINE_TTL_DAYS: u64 = 14;

#[derive(Debug)]
pub struct Checkout {
    pub worker: String,
    pub run_id: String,
    /// The gated host-repo connection this checkout serves ("knowledge" for
    /// the legacy per-worker repo, whether implicit or migrated to a file).
    pub connection: String,
    /// The canonical repo (bare) this clone landed from and pushes to.
    pub bare: PathBuf,
    /// The run's clone (a real repository, `.git` included).
    pub path: PathBuf,
    pub base_commit: String,
    pub knowledge_policy: KnowledgePolicy,
    /// False when the run carried interaction content and this repo's write
    /// contract is "clean-room": the clone mounts read-only and the push
    /// refuses. The enforcement point for the person-space boundary is the
    /// ref write, backed by the ro mount.
    pub writable: bool,
}

impl Checkout {
    pub fn knowledge_mount(&self) -> String {
        format!("{MNT_BASE}/{}", self.connection)
    }

    pub fn origin_mount(&self) -> String {
        format!("{MNT_BASE}/.origins/{}", self.connection)
    }

    pub fn branch(&self) -> String {
        run_branch(&self.run_id)
    }

    pub fn mode_str(&self) -> &'static str {
        if self.writable {
            "write"
        } else {
            "read"
        }
    }
}

#[derive(Debug)]
pub struct RunStorage {
    /// One checkout per gated host-repo connection granted to the worker
    /// ("knowledge" first when present — it's the one the briefing narrates).
    pub repos: Vec<Checkout>,
}

/// A gated host-repo connection granted to the worker: name, canonical bare
/// repo, and — when the connection file declares one — its own `write_from`
/// contract overriding the org `[knowledge]` default.
struct GatedSpec {
    connection: String,
    bare: PathBuf,
    write_from: Option<String>,
}

/// The gated repos a worker's runs provision: every gated host-repo
/// connection granted to it, plus the legacy per-worker knowledge repo as an
/// implicit connection named "knowledge" while no connection file claims that
/// name — so pre-migration deployments keep working untouched.
fn gated_specs(worker: &str) -> Vec<GatedSpec> {
    let mut specs: Vec<GatedSpec> = Vec::new();
    if let Ok(config) = crate::config::snapshot() {
        for m in &config.host_mounts {
            if let crate::config::HostMountKind::Repo {
                gated: true,
                write_from,
                ..
            } = &m.kind
            {
                if m.applies_to(worker) {
                    specs.push(GatedSpec {
                        connection: m.name.clone(),
                        bare: m.path.clone(),
                        write_from: write_from.clone(),
                    });
                }
            }
        }
    }
    let legacy = paths::worker_knowledge_dir(worker).join("repo.git");
    if legacy.join("HEAD").is_file() && !specs.iter().any(|s| s.connection == "knowledge") {
        specs.insert(
            0,
            GatedSpec {
                connection: "knowledge".into(),
                bare: legacy,
                write_from: None,
            },
        );
    } else if let Some(pos) = specs.iter().position(|s| s.connection == "knowledge") {
        let s = specs.remove(pos);
        specs.insert(0, s);
    }
    specs
}

/// Does the participant scan have anything to protect for this worker? The
/// scan polices the `file_task` bridge into clean-room runs holding writable
/// gated clones — a worker with no gated repo granted has no such runs, and
/// the scan does not engage (docs/repos.md).
pub fn boundary_applies(worker: &str) -> bool {
    let worker = short_worker(worker);
    if safe_component(worker, "worker").is_err() {
        return true; // fail closed: a malformed name never disables the scan
    }
    crate::worker::storage::load(worker).knowledge.enabled && !gated_specs(worker).is_empty()
}

/// What a landed push did, reported back to the box as the action result.
#[derive(Debug)]
pub struct PushOutcome {
    pub commit: String,
    pub files: usize,
    pub deletions: usize,
}

/// A held integration lane. Backed by an `flock`, so the OS releases it if the
/// holder crashes.
#[derive(Debug)]
struct Lease {
    _lock: crate::statefile::FileLock,
}

struct TempTree {
    path: PathBuf,
    remove: bool,
}

impl TempTree {
    fn new(parent: &Path, label: &str) -> Result<Self, String> {
        let path = parent.join(format!(
            ".tmp-{label}-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ));
        fs::create_dir_all(&path).map_err(|error| error.to_string())?;
        Ok(Self { path, remove: true })
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        if self.remove {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileData {
    bytes: Vec<u8>,
}

pub fn provision(
    worker: &str,
    run_id: &str,
    context: &crate::worker::memory::RunContext,
) -> Result<RunStorage, String> {
    let worker = short_worker(worker);
    safe_component(worker, "worker")?;
    safe_component(run_id, "run id")?;
    let policy = crate::worker::storage::load(worker);

    if !policy.knowledge.enabled {
        crate::run::runlog::attach_storage(run_id, Default::default())?;
        return Ok(RunStorage { repos: Vec::new() });
    }

    let mut repos: Vec<Checkout> = Vec::new();
    let mut records: BTreeMap<String, KnowledgeRunRecord> = BTreeMap::new();
    for GatedSpec {
        connection,
        bare,
        write_from,
    } in gated_specs(worker)
    {
        safe_component(&connection, "connection")?;
        // The clean-room rule, evaluated here — the host-repo path is the
        // predicate's owner: a run that carried interaction content never
        // gets a writable clone under "clean-room". The run's provenance
        // decides, not the model; the connection's own write_from beats the
        // org [knowledge] default.
        let contract = write_from
            .as_deref()
            .unwrap_or(policy.knowledge.write_from.as_str());
        let writable = !(context.carries_interaction() && contract == "clean-room");
        prune_quarantine(&bare);
        let path = paths::run_dir(run_id).join("repos").join(&connection);
        if path.exists() {
            return Err(format!(
                "repo checkout already exists at {}",
                path.display()
            ));
        }
        // --no-hardlinks is load-bearing: a default local clone hardlinks
        // object files, so the run clone (bind-mounted rw into the box,
        // running as the host uid) would share inodes with the canonical
        // store — a box could chmod +w a pack and corrupt canonical bytes
        // THROUGH its own clone, bypassing the read-only origin mount.
        // Copying severs that alias; the host-only transient clones elsewhere
        // keep the cheap default.
        run_git_owned(
            Path::new("."),
            vec![
                "clone".into(),
                "--quiet".into(),
                "--no-hardlinks".into(),
                bare.display().to_string(),
                path.display().to_string(),
            ],
        )?;
        let base_commit = run_git(&path, &["rev-parse", "HEAD"])?;
        run_git_owned(
            &path,
            vec![
                "checkout".into(),
                "--quiet".into(),
                "-b".into(),
                run_branch(run_id),
            ],
        )?;
        // The worker authors its own history; commits on main carry its name.
        run_git(&path, &["config", "user.name", worker])?;
        run_git_owned(
            &path,
            vec![
                "config".into(),
                "user.email".into(),
                format!("{worker}@workers.roster.local"),
            ],
        )?;
        let checkout = Checkout {
            worker: worker.into(),
            run_id: run_id.into(),
            connection: connection.clone(),
            bare,
            path: path.clone(),
            base_commit: base_commit.clone(),
            knowledge_policy: policy.knowledge.clone(),
            writable,
        };
        // Inside the box, origin is the read-only mount of the canonical
        // repo — live, fetchable; ref writes are a filesystem error.
        let origin = checkout.origin_mount();
        run_git(&path, &["remote", "set-url", "origin", &origin])?;
        records.insert(
            connection,
            KnowledgeRunRecord {
                base_commit,
                mode: if writable { "write" } else { "read" }.into(),
                state: if writable { "active" } else { "read-only" }.into(),
                produced_commit: None,
                error: None,
            },
        );
        repos.push(checkout);
    }
    crate::run::runlog::attach_storage(run_id, records)?;
    Ok(RunStorage { repos })
}

// ── the push: validate a bundled range, advance main ff-only ────────────────

/// Land a run branch on main. `head` is the box's claimed tip; the bundle at
/// the well-known path inside the run's clone is the transfer. The host never
/// runs git against the box-written clone — a box-written `.git/config` is an
/// execution vector — so everything arrives through the inert bundle,
/// quarantined and validated before any canonical ref moves.
pub fn push(
    worker: &str,
    run_id: &str,
    connection: &str,
    head: &str,
    confirmed_bulk_delete: bool,
) -> Result<PushOutcome, String> {
    let worker = short_worker(worker);
    safe_component(worker, "worker")?;
    safe_component(run_id, "run id")?;
    safe_component(connection, "connection")?;
    if !head.bytes().all(|b| b.is_ascii_hexdigit()) || head.len() != 40 {
        return Err("head must be a full commit sha".into());
    }
    let record = crate::run::runlog::load(run_id)
        .map(|record| record.repos)
        .unwrap_or_default()
        .remove(connection)
        .ok_or_else(|| format!("this run has no \"{connection}\" repo checkout"))?;
    if record.mode != "write" {
        return Err("this run's repo checkout is read-only — durable research belongs in a clean task run (file_task)".into());
    }
    let bare = gated_specs(worker)
        .into_iter()
        .find(|s| s.connection == connection)
        .map(|s| s.bare)
        .ok_or_else(|| format!("no gated repo connection \"{connection}\" for this worker"))?;
    let policy = crate::worker::storage::load(worker).knowledge;
    match push_inner(
        worker,
        run_id,
        connection,
        &bare,
        head,
        confirmed_bulk_delete,
        &policy,
    ) {
        Ok(outcome) => {
            crate::run::runlog::update_knowledge(
                run_id,
                connection,
                "pushed",
                Some(&outcome.commit),
                None,
            )?;
            Ok(outcome)
        }
        Err(error) => {
            let _ = crate::run::runlog::update_knowledge(
                run_id,
                connection,
                "push-refused",
                None,
                Some(&error),
            );
            let _ = crate::worker::journal::append_required(
                &journal_worker(worker),
                run_id,
                "knowledge-push-refused",
                json!({ "connection": connection, "head": head, "error": error }),
            );
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn push_inner(
    worker: &str,
    run_id: &str,
    connection: &str,
    bare: &Path,
    head: &str,
    confirmed_bulk_delete: bool,
    policy: &KnowledgePolicy,
) -> Result<PushOutcome, String> {
    let repo = bare.to_path_buf();
    let bundle = paths::run_dir(run_id)
        .join("repos")
        .join(connection)
        .join(".git")
        .join(PUSH_BUNDLE);
    if !bundle.exists() {
        return Err(
            "no push bundle found — the repo_push tool creates it from your committed branch"
                .into(),
        );
    }
    let bundle_bytes = fs::metadata(&bundle)
        .map_err(|error| error.to_string())?
        .len();
    if bundle_bytes > policy.max_repo_bytes {
        return Err(format!(
            "push bundle is {bundle_bytes} bytes, over the {} byte repository limit",
            policy.max_repo_bytes
        ));
    }

    // Quarantine: a bare clone of the canonical repo (so thin-bundle
    // prerequisites resolve), which receives the bundle and hosts every check.
    let worker_dir = worker_dir(worker);
    let quarantine = TempTree::new(&worker_dir, "push")?;
    let q = quarantine.path.join("quarantine.git");
    run_git_owned(
        Path::new("."),
        vec![
            "clone".into(),
            "--quiet".into(),
            "--bare".into(),
            repo.display().to_string(),
            q.display().to_string(),
        ],
    )?;
    git_dir(
        &q,
        &["bundle", "verify", "--quiet", &bundle.display().to_string()],
    )
    .map_err(|error| format!("push bundle failed verification: {error}"))?;
    git_dir(
        &q,
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            &bundle.display().to_string(),
            "HEAD:refs/q/head",
        ],
    )?;
    let fetched = git_dir(&q, &["rev-parse", "refs/q/head"])?;
    if fetched != head {
        return Err(format!(
            "the bundle's head {fetched} does not match the proposed head {head} — recreate the bundle and push again"
        ));
    }
    git_dir(&q, &["fsck", "--no-progress"])
        .map_err(|error| format!("pushed objects failed fsck: {error}"))?;

    // The whole proposed tree: regular non-executable files only, within the
    // size budget, on acceptable paths.
    let mut total_bytes: u64 = 0;
    for line in git_dir(&q, &["ls-tree", "-r", "--long", "refs/q/head"])?.lines() {
        let (meta, path) = line
            .split_once('\t')
            .ok_or_else(|| format!("unparseable ls-tree line: {line}"))?;
        let fields: Vec<&str> = meta.split_whitespace().collect();
        let (mode, size) = match fields.as_slice() {
            [mode, _type, _sha, size] => (*mode, *size),
            _ => return Err(format!("unparseable ls-tree line: {line}")),
        };
        if mode != "100644" {
            return Err(format!(
                "knowledge may contain only regular files (mode 100644): {path} has mode {mode}"
            ));
        }
        total_bytes += size.parse::<u64>().unwrap_or(0);
        validate_relative_path(Path::new(path))?;
    }
    if total_bytes > policy.max_repo_bytes {
        return Err(format!(
            "pushed tree is {total_bytes} bytes, over the {} byte limit",
            policy.max_repo_bytes
        ));
    }

    // Fast-forward or stale — the host never merges content.
    let main = head_of(&repo)?;
    if head == main {
        return Ok(PushOutcome {
            commit: head.into(),
            files: 0,
            deletions: 0,
        });
    }
    let stale = |main: &str| {
        format!(
            "stale: main is now {main} — fetch origin, rebase your branch onto origin/main, and push again"
        )
    };
    if !is_ancestor(&q, &main, head) {
        return Err(stale(&main));
    }

    // Validate what changed (the already-landed history was validated when it
    // landed): file contents, secrets, and the person-space boundary scan.
    let context = crate::worker::memory::load_run_context(run_id);
    let markers = crate::worker::boundary::participant_markers(&context);
    let mut files = 0usize;
    let mut deletions = 0usize;
    for line in git_dir(&q, &["diff", "--raw", "--no-renames", &main, "refs/q/head"])?.lines() {
        let (meta, path) = line
            .split_once('\t')
            .ok_or_else(|| format!("unparseable diff line: {line}"))?;
        let fields: Vec<&str> = meta.split_whitespace().collect();
        let [_src_mode, _dst_mode, _src_sha, dst_sha, status] = fields.as_slice() else {
            return Err(format!("unparseable diff line: {line}"));
        };
        files += 1;
        if *status == "D" {
            deletions += 1;
            continue;
        }
        let bytes = git_dir_bytes(&q, &["cat-file", "blob", dst_sha])?;
        validate_text_file(Path::new(path), &bytes, policy)?;
        if let Ok(text) = std::str::from_utf8(&bytes) {
            if let Some(hit) = crate::worker::boundary::scan(text, &markers, false) {
                return Err(format!(
                    "{path} references a conversation participant (\"{hit}\") — that belongs in memory, not in knowledge"
                ));
            }
        }
    }
    if deletions > policy.max_deletions_ungated && !confirmed_bulk_delete {
        return Err(format!(
            "this push deletes {deletions} files — over the ungated limit of {}. If that is intended, \
             propose repo_push again with confirm_bulk_delete: \"yes\" and a rationale; that path \
             waits for human approval",
            policy.max_deletions_ungated
        ));
    }

    // The integration lane: land atomically, re-checking main under the lock.
    // Keyed by the canonical repo's path, not the worker — an org-granted
    // repo has ONE lane no matter how many workers push to it.
    let _lane = acquire_lease(&lane_lock_path(&repo))?;
    let main = head_of(&repo)?;
    if head != main && !is_ancestor(&q, &main, head) {
        return Err(stale(&main));
    }
    let incoming = format!("refs/roster/incoming/{run_id}");
    git_dir(
        &repo,
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            &q.display().to_string(),
            &format!("refs/q/head:{incoming}"),
        ],
    )?;
    // Compare-and-swap: refuses if main moved between the check and the write.
    let advance = git_dir(&repo, &["update-ref", "refs/heads/main", head, &main]);
    let _ = git_dir(&repo, &["update-ref", "-d", &incoming]);
    advance.map_err(|_| stale(&head_of(&repo).unwrap_or_default()))?;

    crate::worker::journal::append_required(
        &journal_worker(worker),
        run_id,
        "knowledge-pushed",
        json!({
            "connection": connection,
            "previous_main": main,
            "commit": head,
            "files": files,
            "deletions": deletions,
        }),
    )?;
    Ok(PushOutcome {
        commit: head.into(),
        files,
        deletions,
    })
}

// ── the exit backstop: park unlanded work on a quarantine ref ────────────────

/// Called when a writable run ends, however it ends. Whatever the worktree
/// holds beyond the last landed state is snapshotted — by hashing the files,
/// never by reading the box's `.git` — onto `refs/quarantine/run-<id>` so
/// nothing is lost silently; the next run's briefing points at it.
pub fn backstop(checkout: &Checkout) {
    if !checkout.writable {
        return;
    }
    if let Err(error) = backstop_inner(checkout) {
        let _ = crate::run::runlog::update_knowledge(
            &checkout.run_id,
            &checkout.connection,
            "backstop-failed",
            None,
            Some(&error),
        );
        let _ = crate::worker::journal::append_required(
            &journal_worker(&checkout.worker),
            &checkout.run_id,
            "knowledge-backstop-failed",
            json!({ "error": error }),
        );
    }
}

fn backstop_inner(checkout: &Checkout) -> Result<(), String> {
    let repo = checkout.bare.clone();
    // The last landed state this run is responsible for: its pushed head if a
    // push landed, else the commit it started from.
    let reference = crate::run::runlog::load(&checkout.run_id)
        .map(|record| record.repos)
        .unwrap_or_default()
        .remove(&checkout.connection)
        .and_then(|k| k.produced_commit)
        .unwrap_or_else(|| checkout.base_commit.clone());
    let current = collect_files_lenient(&checkout.path)?;
    let total: u64 = current.values().map(|f| f.bytes.len() as u64).sum();
    if total > checkout.knowledge_policy.max_repo_bytes {
        return Err(format!(
            "worktree is {total} bytes, over the {} byte limit — not parked",
            checkout.knowledge_policy.max_repo_bytes
        ));
    }
    let worker_dir = worker_dir(&checkout.worker);
    let staging = TempTree::new(&worker_dir, "backstop")?;
    let tree = staging.path.join("tree");
    clone_at(&repo, &tree, &reference)?;
    if collect_files_lenient(&tree)? == current {
        return Ok(()); // everything this run did is already on main
    }
    // Rebuild the worktree state on a host-owned clone: clear, overlay, commit.
    for entry in fs::read_dir(&tree).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        if entry.file_name() == ".git" {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            fs::remove_dir_all(&path).map_err(|error| error.to_string())?;
        } else {
            fs::remove_file(&path).map_err(|error| error.to_string())?;
        }
    }
    for (relative, file) in &current {
        let destination = tree.join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        fs::write(destination, &file.bytes).map_err(|error| error.to_string())?;
    }
    run_git(&tree, &["config", "user.name", "Roster Backstop"])?;
    run_git(&tree, &["config", "user.email", "backstop@roster.local"])?;
    run_git(&tree, &["add", "--all"])?;
    run_git_owned(
        &tree,
        vec![
            "commit".into(),
            "--quiet".into(),
            "-m".into(),
            format!(
                "Backstop: unpushed knowledge from run {}\n\nRoster-Worker: {}\nRoster-Run: {}\nRoster-Reference: {}",
                checkout.run_id, checkout.worker, checkout.run_id, reference
            ),
        ],
    )?;
    let quarantine_ref = quarantine_ref(&checkout.run_id);
    run_git_owned(
        &tree,
        vec![
            "push".into(),
            "--quiet".into(),
            "origin".into(),
            format!("HEAD:{quarantine_ref}"),
        ],
    )?;
    crate::run::runlog::update_knowledge(
        &checkout.run_id,
        &checkout.connection,
        "backstopped",
        None,
        None,
    )?;
    crate::worker::journal::append_required(
        &journal_worker(&checkout.worker),
        &checkout.run_id,
        "knowledge-backstopped",
        json!({ "connection": checkout.connection, "ref": quarantine_ref, "reference": reference }),
    )?;
    Ok(())
}

/// Parked refs across a worker's gated repos: (connection, ref name, age in
/// days) — the briefing's source.
pub fn parked_runs(worker: &str) -> Vec<(String, String, u64)> {
    gated_specs(short_worker(worker))
        .into_iter()
        .flat_map(|spec| {
            parked_refs_of(&spec.bare)
                .into_iter()
                .map(move |(name, age)| (spec.connection.clone(), name, age))
        })
        .collect()
}

fn prune_quarantine(repo: &Path) {
    for (name, age_days) in parked_refs_of(repo) {
        if age_days > QUARANTINE_TTL_DAYS {
            let _ = git_dir(repo, &["update-ref", "-d", &name]);
        }
    }
}

fn parked_refs_of(repo: &Path) -> Vec<(String, u64)> {
    let Ok(out) = git_dir(
        repo,
        &[
            "for-each-ref",
            "refs/quarantine",
            "--format=%(refname)%09%(creatordate:unix)",
        ],
    ) else {
        return Vec::new();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    out.lines()
        .filter_map(|line| {
            let (name, stamp) = line.split_once('\t')?;
            let age_days = now.saturating_sub(stamp.parse().unwrap_or(now)) / 86_400;
            Some((name.to_string(), age_days))
        })
        .collect()
}

// ── validation kept from the mode era: content, paths, secrets ───────────────

fn validate_text_file(path: &Path, bytes: &[u8], policy: &KnowledgePolicy) -> Result<(), String> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    if !ALLOWED_EXTENSIONS.contains(&extension) {
        return Err(format!(
            "unsupported knowledge extension for {} (allowed: {})",
            path.display(),
            ALLOWED_EXTENSIONS.join(", ")
        ));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| format!("knowledge file must be UTF-8 text: {}", path.display()))?;
    if text.chars().count() > policy.max_file_chars {
        return Err(format!(
            "knowledge file exceeds {} characters: {}",
            policy.max_file_chars,
            path.display()
        ));
    }
    if obvious_secret(text) {
        return Err(format!(
            "knowledge file appears to contain a secret: {}",
            path.display()
        ));
    }
    Ok(())
}

fn validate_relative_path(path: &Path) -> Result<(), String> {
    const RESERVED: &[&str] = &[
        ".git",
        ".pi",
        ".codex",
        "AGENTS.md",
        "SKILL.md",
        "identity.md",
        "purpose.md",
        "worker.toml",
    ];
    if path.is_absolute() {
        return Err("knowledge path cannot be absolute".into());
    }
    for component in path.components() {
        let Component::Normal(value) = component else {
            return Err(format!("unsafe knowledge path: {}", path.display()));
        };
        let value = value
            .to_str()
            .ok_or_else(|| format!("knowledge path is not UTF-8: {}", path.display()))?;
        if value.is_empty() || value.starts_with('.') || RESERVED.contains(&value) {
            return Err(format!("reserved knowledge path: {}", path.display()));
        }
    }
    Ok(())
}

fn obvious_secret(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "-----begin private key",
        "authorization: bearer",
        "password:",
        "api_key:",
        "api key is ",
        "access token:",
        "ghp_",
        "xoxb-",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Walk a worktree for the backstop: best-effort parking, so symlinks and
/// other oddities are skipped rather than fatal, and `.git` is never read.
fn collect_files_lenient(root: &Path) -> Result<BTreeMap<PathBuf, FileData>, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, FileData>) -> Result<(), String> {
        for entry in fs::read_dir(dir).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .map_err(|error| error.to_string())?
                .to_path_buf();
            if relative.components().next() == Some(Component::Normal(".git".as_ref())) {
                continue;
            }
            let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                walk(root, &path, out)?;
            } else if metadata.is_file() {
                out.insert(
                    relative,
                    FileData {
                        bytes: fs::read(&path).map_err(|error| error.to_string())?,
                    },
                );
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out)?;
    Ok(out)
}

// ── repo plumbing ─────────────────────────────────────────────────────────────

/// The per-repo integration lane lock, keyed by the canonical repo's path
/// under `state/locks/` — never a file beside the repo itself, which for a
/// granted host repo would mean littering the operator's directory.
fn lane_lock_path(bare: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bare.display().to_string().as_bytes());
    let digest = format!("{:x}", h.finalize());
    paths::lock_file(&format!("repo-lane-{}", &digest[..16]))
}

fn ensure_repo(worker: &str) -> Result<PathBuf, String> {
    let dir = worker_dir(worker);
    fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let _lease = acquire_lease(&lane_lock_path(&canonical_repo(worker)))?;
    let repo = canonical_repo(worker);
    if repo.join("refs/heads/main").exists() || head_of(&repo).is_ok() {
        return Ok(repo);
    }
    if repo.exists() {
        return Err(format!(
            "knowledge repository exists without main: {}",
            repo.display()
        ));
    }
    run_git_owned(
        Path::new("."),
        vec![
            "init".into(),
            "--bare".into(),
            "--initial-branch=main".into(),
            repo.display().to_string(),
        ],
    )?;
    // Concurrent readers (box fetches) vs auto-gc repacks don't mix; the host
    // gc's explicitly if it ever needs to.
    git_dir(&repo, &["config", "gc.auto", "0"])?;
    let init = TempTree::new(&dir, "init")?;
    let tree = init.path.join("tree");
    clone_repo(&repo, &tree)?;
    fs::write(
        tree.join("README.md"),
        "# Knowledge\n\nThis repository is the worker's durable knowledge about the world. \
         The layout is the worker's own to shape; every change lands through a pushed, \
         host-validated run branch, and `git log main` is the audit trail.\n",
    )
    .map_err(|error| error.to_string())?;
    run_git(&tree, &["config", "user.name", "Roster Knowledge"])?;
    run_git(&tree, &["config", "user.email", "knowledge@roster.local"])?;
    run_git(&tree, &["add", "--all"])?;
    run_git(
        &tree,
        &["commit", "-m", "Initialize worker knowledge repository"],
    )?;
    run_git(&tree, &["push", "--quiet", "origin", "main"])?;
    Ok(repo)
}

fn acquire_lease(path: &Path) -> Result<Lease, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    if path.is_dir() {
        let _ = fs::remove_dir_all(path); // pre-flock scheme left a directory
    }
    // Bounded wait (~5s) on a lock the OS frees on crash.
    for _ in 0..250 {
        match crate::statefile::FileLock::try_acquire_path(path) {
            Ok(Some(lock)) => return Ok(Lease { _lock: lock }),
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(error) => return Err(error.to_string()),
        }
    }
    Err(format!(
        "timed out waiting for knowledge integration lane at {}",
        path.display()
    ))
}

fn clone_repo(repo: &Path, destination: &Path) -> Result<(), String> {
    if destination.exists() {
        fs::remove_dir_all(destination).map_err(|error| error.to_string())?;
    }
    run_git_owned(
        Path::new("."),
        vec![
            "clone".into(),
            "--quiet".into(),
            repo.display().to_string(),
            destination.display().to_string(),
        ],
    )?;
    Ok(())
}

fn clone_at(repo: &Path, destination: &Path, commit: &str) -> Result<(), String> {
    clone_repo(repo, destination)?;
    run_git_owned(
        destination,
        vec![
            "checkout".into(),
            "--quiet".into(),
            "--detach".into(),
            commit.into(),
        ],
    )?;
    Ok(())
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    run_git_owned(cwd, args.iter().map(|value| (*value).to_string()).collect())
}

fn run_git_owned(cwd: &Path, args: Vec<String>) -> Result<String, String> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(&args)
        .output()
        .map_err(|error| format!("could not run git: {error}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!("git {} failed: {detail}", args.join(" ")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Run git against a repository by `--git-dir`, from a neutral cwd.
fn git_dir(repo: &Path, args: &[&str]) -> Result<String, String> {
    let mut owned: Vec<String> = vec![format!("--git-dir={}", repo.display())];
    owned.extend(args.iter().map(|value| (*value).to_string()));
    run_git_owned(Path::new("."), owned)
}

/// Like `git_dir` but returns raw bytes (blob contents may not be UTF-8).
fn git_dir_bytes(repo: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new("git")
        .arg(format!("--git-dir={}", repo.display()))
        .args(args)
        .output()
        .map_err(|error| format!("could not run git: {error}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!("git {} failed: {detail}", args.join(" ")));
    }
    Ok(output.stdout)
}

fn is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> bool {
    git_dir(repo, &["merge-base", "--is-ancestor", ancestor, descendant]).is_ok()
}

fn head_of(repo: &Path) -> Result<String, String> {
    git_dir(repo, &["rev-parse", "refs/heads/main"])
}

fn run_branch(run_id: &str) -> String {
    format!("run/{run_id}")
}

fn quarantine_ref(run_id: &str) -> String {
    format!("refs/quarantine/run-{run_id}")
}

fn short_worker(worker: &str) -> &str {
    worker.strip_prefix("org/").unwrap_or(worker)
}

fn journal_worker(worker: &str) -> String {
    format!("org/{}", short_worker(worker))
}

fn worker_dir(worker: &str) -> PathBuf {
    paths::worker_knowledge_dir(worker)
}

fn canonical_repo(worker: &str) -> PathBuf {
    worker_dir(worker).join("repo.git")
}

fn safe_component(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(format!("unsafe {label} \"{value}\""));
    }
    Ok(())
}

pub fn initialize(worker: &str) -> Result<String, String> {
    let worker = short_worker(worker);
    safe_component(worker, "worker")?;
    let repo = ensure_repo(worker)?;
    head_of(&repo)
}

pub fn repo_path(worker: &str) -> Result<PathBuf, String> {
    let worker = short_worker(worker);
    safe_component(worker, "worker")?;
    let repo = canonical_repo(worker);
    if !repo.exists() {
        return Err(format!(
            "knowledge repository for {worker} is not initialized; create the worker first: roster worker add {worker}"
        ));
    }
    Ok(repo)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> KnowledgePolicy {
        KnowledgePolicy {
            max_file_chars: 1_000,
            max_repo_bytes: 100_000,
            ..Default::default()
        }
    }

    fn git_env_sandbox() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn walkdir(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.push(path);
                }
            }
        }
        out
    }

    /// A canonical repo + a "box" clone, exercising the real push path:
    /// provisioning shape, bundle transfer, validation, ff-only advance.
    fn scaffold(dir: &Path) -> (PathBuf, PathBuf) {
        let repo = dir.join("repo.git");
        run_git_owned(
            Path::new("."),
            vec![
                "init".into(),
                "--bare".into(),
                "--initial-branch=main".into(),
                repo.display().to_string(),
            ],
        )
        .unwrap();
        let seed = dir.join("seed");
        clone_repo(&repo, &seed).unwrap();
        std::fs::write(seed.join("README.md"), "# Knowledge\n").unwrap();
        run_git(&seed, &["config", "user.name", "t"]).unwrap();
        run_git(&seed, &["config", "user.email", "t@t"]).unwrap();
        run_git(&seed, &["add", "--all"]).unwrap();
        run_git(&seed, &["commit", "-q", "-m", "init"]).unwrap();
        run_git(&seed, &["push", "-q", "origin", "main"]).unwrap();
        let clone = dir.join("box-clone");
        clone_repo(&repo, &clone).unwrap();
        run_git(&clone, &["checkout", "-q", "-b", "run/test"]).unwrap();
        run_git(&clone, &["config", "user.name", "dobby"]).unwrap();
        run_git(&clone, &["config", "user.email", "y@y"]).unwrap();
        (repo, clone)
    }

    fn commit_file(clone: &Path, path: &str, contents: &str, message: &str) -> String {
        let full = clone.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, contents).unwrap();
        run_git(clone, &["add", "--all"]).unwrap();
        run_git(clone, &["commit", "-q", "-m", message]).unwrap();
        run_git(clone, &["rev-parse", "HEAD"]).unwrap()
    }

    fn bundle(clone: &Path) -> PathBuf {
        let file = clone.join(".git").join(PUSH_BUNDLE);
        run_git_owned(
            clone,
            vec![
                "bundle".into(),
                "create".into(),
                file.display().to_string(),
                "origin/main..HEAD".into(),
            ],
        )
        .unwrap();
        file
    }

    /// The quarantine → validate → ff-advance core, driven directly (the
    /// public `push` wraps it with runlog/journal IO that needs a deployment).
    fn land(
        repo: &Path,
        clone: &Path,
        head: &str,
        confirmed: bool,
    ) -> Result<(usize, usize), String> {
        let file = bundle(clone);
        // Inline replica of push_inner's quarantine/validate/land sequence,
        // minus run-record IO: keeps the test on the pure git mechanics.
        let q_parent = TempTree::new(repo.parent().unwrap(), "test-push").unwrap();
        let q = q_parent.path.join("q.git");
        run_git_owned(
            Path::new("."),
            vec![
                "clone".into(),
                "--quiet".into(),
                "--bare".into(),
                repo.display().to_string(),
                q.display().to_string(),
            ],
        )?;
        git_dir(
            &q,
            &["bundle", "verify", "--quiet", &file.display().to_string()],
        )?;
        git_dir(
            &q,
            &[
                "fetch",
                "--quiet",
                "--no-tags",
                &file.display().to_string(),
                "HEAD:refs/q/head",
            ],
        )?;
        assert_eq!(git_dir(&q, &["rev-parse", "refs/q/head"]).unwrap(), head);
        let p = policy();
        for line in git_dir(&q, &["ls-tree", "-r", "--long", "refs/q/head"])?.lines() {
            let (meta, path) = line.split_once('\t').unwrap();
            let fields: Vec<&str> = meta.split_whitespace().collect();
            if fields[0] != "100644" {
                return Err(format!("mode {}: {path}", fields[0]));
            }
            validate_relative_path(Path::new(path))?;
        }
        let main = head_of(repo)?;
        if !is_ancestor(&q, &main, head) {
            return Err(format!("stale: main is now {main}"));
        }
        let mut files = 0;
        let mut deletions = 0;
        for line in git_dir(&q, &["diff", "--raw", "--no-renames", &main, "refs/q/head"])?.lines() {
            let (meta, path) = line.split_once('\t').unwrap();
            let fields: Vec<&str> = meta.split_whitespace().collect();
            files += 1;
            if fields[4] == "D" {
                deletions += 1;
                continue;
            }
            let bytes = git_dir_bytes(&q, &["cat-file", "blob", fields[3]])?;
            validate_text_file(Path::new(path), &bytes, &p)?;
        }
        if deletions > p.max_deletions_ungated && !confirmed {
            return Err(format!("deletes {deletions} files ungated"));
        }
        git_dir(
            repo,
            &[
                "fetch",
                "--quiet",
                "--no-tags",
                &q.display().to_string(),
                "refs/q/head:refs/roster/incoming/test",
            ],
        )?;
        git_dir(repo, &["update-ref", "refs/heads/main", head, &main])?;
        let _ = git_dir(repo, &["update-ref", "-d", "refs/roster/incoming/test"]);
        Ok((files, deletions))
    }

    #[test]
    fn push_lands_fast_forward_and_refuses_stale() {
        let dir = git_env_sandbox();
        let (repo, clone) = scaffold(dir.path());
        let head = commit_file(&clone, "notes/first.md", "# First\n", "add first");
        let (files, deletions) = land(&repo, &clone, &head, false).unwrap();
        assert_eq!((files, deletions), (1, 0));
        assert_eq!(head_of(&repo).unwrap(), head);

        // A second clone that never rebased is stale once main moved.
        let other = dir.path().join("other-clone");
        clone_repo(&repo, &other).unwrap();
        run_git(&other, &["checkout", "-q", "HEAD~1"]).unwrap();
        run_git(&other, &["checkout", "-q", "-b", "run/other"]).unwrap();
        run_git(&other, &["config", "user.name", "dobby"]).unwrap();
        run_git(&other, &["config", "user.email", "y@y"]).unwrap();
        let stale_head = commit_file(&other, "notes/second.md", "# Second\n", "add second");
        let error = land(&repo, &other, &stale_head, false).unwrap_err();
        assert!(error.contains("stale"), "{error}");

        // After a rebase onto main it lands.
        run_git(&other, &["fetch", "-q", "origin"]).unwrap();
        run_git(&other, &["rebase", "-q", "origin/main"]).unwrap();
        let rebased = run_git(&other, &["rev-parse", "HEAD"]).unwrap();
        land(&repo, &other, &rebased, false).unwrap();
        assert_eq!(head_of(&repo).unwrap(), rebased);
    }

    #[test]
    fn push_gate_catches_bulk_deletions_and_edits_and_deletes_are_legal() {
        let dir = git_env_sandbox();
        let (repo, clone) = scaffold(dir.path());
        let mut head = String::new();
        for i in 0..25 {
            head = commit_file(&clone, &format!("notes/n{i}.md"), "x\n", "seed");
        }
        land(&repo, &clone, &head, false).unwrap();

        // Edits and small deletes are ordinary now — the motivating incident.
        std::fs::remove_file(clone.join("notes/n0.md")).unwrap();
        std::fs::write(clone.join("notes/n1.md"), "rewritten\n").unwrap();
        run_git(&clone, &["add", "--all"]).unwrap();
        run_git(&clone, &["commit", "-q", "-m", "prune and rewrite"]).unwrap();
        let head = run_git(&clone, &["rev-parse", "HEAD"]).unwrap();
        let (files, deletions) = land(&repo, &clone, &head, false).unwrap();
        assert_eq!((files, deletions), (2, 1));

        // A bulk wipe needs confirmation (which routes to a human gate).
        for i in 2..25 {
            let _ = std::fs::remove_file(clone.join(format!("notes/n{i}.md")));
        }
        run_git(&clone, &["add", "--all"]).unwrap();
        run_git(&clone, &["commit", "-q", "-m", "wipe"]).unwrap();
        let head = run_git(&clone, &["rev-parse", "HEAD"]).unwrap();
        let error = land(&repo, &clone, &head, false).unwrap_err();
        assert!(error.contains("deletes"), "{error}");
        land(&repo, &clone, &head, true).unwrap();
    }

    /// The real lifecycle against a sandboxed deployment: provision a clone,
    /// commit, bundle exactly as the box tool does, land via push(), go stale,
    /// rebase, land again, then park uncommitted leftovers via backstop().
    #[test]
    fn provision_push_stale_rebase_and_backstop_lifecycle() {
        let guard = crate::statefile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ROSTER_ROOT", dir.path());
        std::fs::create_dir_all(dir.path().join("config/workers/dobby")).unwrap();
        std::fs::write(
            dir.path().join("config/workers/dobby/worker.toml"),
            "name = \"dobby\"\n",
        )
        .unwrap();

        initialize("dobby").unwrap();
        crate::run::runlog::start("run1", "dobby", "task", None).unwrap();
        let storage = provision(
            "dobby",
            "run1",
            &crate::worker::memory::RunContext::default(),
        )
        .unwrap();
        let co = storage.repos.first().unwrap();
        assert_eq!(co.connection, "knowledge");
        assert!(co.writable);
        assert_eq!(
            run_git(&co.path, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap(),
            "run/run1"
        );
        // The box-mounted clone must share NO inodes with the canonical store:
        // a hardlinked object would let the box corrupt canonical bytes
        // through its own rw mount, bypassing the read-only origin mount.
        {
            use std::os::unix::fs::MetadataExt;
            for entry in walkdir(&co.path.join(".git").join("objects")) {
                assert_eq!(
                    std::fs::metadata(&entry).unwrap().nlink(),
                    1,
                    "hardlinked object in the run clone: {}",
                    entry.display()
                );
            }
        }

        // Commit and land, exactly as the box tool would.
        let head = commit_file(&co.path, "topics/llms.md", "# LLMs\n", "add a topic");
        run_git_owned(
            &co.path,
            vec![
                "bundle".into(),
                "create".into(),
                co.path.join(".git").join(PUSH_BUNDLE).display().to_string(),
                "origin/main..HEAD".into(),
            ],
        )
        .unwrap();
        let outcome = push("dobby", "run1", "knowledge", &head, false).unwrap();
        assert_eq!((outcome.files, outcome.deletions), (1, 0));
        assert_eq!(head_of(&canonical_repo("dobby")).unwrap(), head);

        // A second run provisioned earlier goes stale, rebases, lands.
        crate::run::runlog::start("run2", "dobby", "task", None).unwrap();
        let storage2 = provision(
            "dobby",
            "run2",
            &crate::worker::memory::RunContext::default(),
        )
        .unwrap();
        let co2 = storage2.repos.first().unwrap();
        // Pretend run2 cloned before run1 landed: rewind its view of main.
        let old_main = run_git(&co2.path, &["rev-list", "--max-parents=0", "HEAD"]).unwrap();
        run_git(&co2.path, &["reset", "--hard", &old_main]).unwrap();
        run_git(
            &co2.path,
            &["update-ref", "refs/remotes/origin/main", &old_main],
        )
        .unwrap();
        let stale_head = commit_file(&co2.path, "topics/agents.md", "# Agents\n", "add");
        run_git_owned(
            &co2.path,
            vec![
                "bundle".into(),
                "create".into(),
                co2.path
                    .join(".git")
                    .join(PUSH_BUNDLE)
                    .display()
                    .to_string(),
                "origin/main..HEAD".into(),
            ],
        )
        .unwrap();
        let error = push("dobby", "run2", "knowledge", &stale_head, false).unwrap_err();
        assert!(error.contains("stale"), "{error}");
        // Rebase against the canonical repo (in the box this is the ro mount).
        run_git_owned(
            &co2.path,
            vec![
                "fetch".into(),
                "--quiet".into(),
                canonical_repo("dobby").display().to_string(),
                "main:refs/remotes/origin/main".into(),
            ],
        )
        .unwrap();
        run_git(&co2.path, &["rebase", "--quiet", "origin/main"]).unwrap();
        let rebased = run_git(&co2.path, &["rev-parse", "HEAD"]).unwrap();
        run_git_owned(
            &co2.path,
            vec![
                "bundle".into(),
                "create".into(),
                co2.path
                    .join(".git")
                    .join(PUSH_BUNDLE)
                    .display()
                    .to_string(),
                "origin/main..HEAD".into(),
            ],
        )
        .unwrap();
        push("dobby", "run2", "knowledge", &rebased, false).unwrap();
        assert_eq!(head_of(&canonical_repo("dobby")).unwrap(), rebased);

        // Leftover uncommitted work parks on a quarantine ref; the next run's
        // briefing source sees it.
        std::fs::write(co2.path.join("topics/unfinished.md"), "wip\n").unwrap();
        backstop(co2);
        let parked = parked_runs("dobby");
        assert_eq!(parked.len(), 1);
        assert_eq!(parked[0].0, "knowledge");
        assert!(parked[0].1.ends_with("run-run2"), "{}", parked[0].1);

        // A run that carried interaction content, under clean-room, gets a
        // read-only clone; push refuses.
        crate::run::runlog::start("run3", "dobby", "task", None).unwrap();
        let storage3 = provision(
            "dobby",
            "run3",
            &crate::worker::memory::RunContext::assume_interaction(),
        )
        .unwrap();
        assert!(!storage3.repos.first().unwrap().writable);
        let error = push("dobby", "run3", "knowledge", &rebased, false).unwrap_err();
        assert!(error.contains("read-only"), "{error}");

        std::env::remove_var("ROSTER_ROOT");
        drop(guard);
    }

    #[test]
    fn boundary_applies_only_with_a_gated_repo() {
        let guard = crate::statefile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ROSTER_ROOT", dir.path());

        // No gated repo granted: the scan has nothing to protect.
        assert!(!boundary_applies("dobby"));
        // The legacy knowledge repo counts as a gated grant.
        initialize("dobby").unwrap();
        assert!(boundary_applies("dobby"));
        // A malformed name never disables the scan.
        assert!(boundary_applies("../oops"));

        std::env::remove_var("ROSTER_ROOT");
        drop(guard);
    }

    #[test]
    fn tree_validation_rejects_bad_modes_paths_and_secrets() {
        let p = policy();
        assert!(validate_relative_path(Path::new("topics/llms.md")).is_ok());
        assert!(validate_relative_path(Path::new(".hidden/x.md")).is_err());
        assert!(validate_relative_path(Path::new("a/worker.toml")).is_err());
        assert!(validate_text_file(Path::new("a.md"), b"fine", &p).is_ok());
        assert!(validate_text_file(Path::new("a.sh"), b"x", &p).is_err());
        assert!(validate_text_file(Path::new("a.md"), b"password: hunter2", &p).is_err());

        let dir = git_env_sandbox();
        let (repo, clone) = scaffold(dir.path());
        std::os::unix::fs::symlink("README.md", clone.join("link.md")).unwrap();
        run_git(&clone, &["add", "--all"]).unwrap();
        run_git(&clone, &["commit", "-q", "-m", "symlink"]).unwrap();
        let head = run_git(&clone, &["rev-parse", "HEAD"]).unwrap();
        let error = land(&repo, &clone, &head, false).unwrap_err();
        assert!(error.contains("mode 120000"), "{error}");
    }
}
