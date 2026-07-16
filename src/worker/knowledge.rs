//! Git-backed world knowledge for a worker. The box receives a plain checkout
//! with no Git metadata. The trusted host validates additions and creates the
//! commit in a serialized per-worker integration lane.

use crate::worker::storage::KnowledgePolicy;
use crate::paths;
use crate::run::runlog::KnowledgeRunRecord;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

const KNOWLEDGE_MOUNT: &str = "/opt/roster/knowledge";
const ALLOWED_EXTENSIONS: &[&str] = &["md", "json", "jsonl", "yaml", "yml", "csv", "txt"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnowledgeMode {
    Append,
    Reorganization,
    /// The shelf, consultation only: mounted `:ro`, no namespace, no
    /// checkpoint. What tainted runs get under the clean-room boundary
    /// (docs/knowledge.md).
    Read,
}

impl KnowledgeMode {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "append" => Ok(Self::Append),
            "reorganization" => Ok(Self::Reorganization),
            "read" => Ok(Self::Read),
            other => Err(format!("unknown knowledge mode \"{other}\"")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Append => "append",
            Self::Reorganization => "reorganization",
            Self::Read => "read",
        }
    }
}

#[derive(Debug)]
pub struct Checkout {
    pub worker: String,
    pub run_id: String,
    pub path: PathBuf,
    pub base_commit: String,
    pub record_namespace: String,
    pub knowledge_policy: KnowledgePolicy,
    pub mode: KnowledgeMode,
}

impl Checkout {
    pub fn knowledge_mount(&self) -> &'static str {
        KNOWLEDGE_MOUNT
    }
}

#[derive(Debug)]
pub struct RunStorage {
    pub worker: String,
    pub run_id: String,
    pub knowledge: Option<Checkout>,
    _reorganization_lease: Option<Lease>,
}

impl Drop for RunStorage {
    fn drop(&mut self) {
        if self._reorganization_lease.is_some() {
            let _ = crate::worker::journal::append_required(
                &journal_worker(&self.worker),
                &self.run_id,
                "knowledge-reorganization-lease-released",
                json!({ "implicit": true }),
            );
            drop(self._reorganization_lease.take());
        }
    }
}

#[derive(Debug)]
pub struct Checkpoint {
    pub state: &'static str,
    pub commit: Option<String>,
    pub files: usize,
}

#[derive(Debug)]
struct CheckpointError {
    state: &'static str,
    message: String,
}

impl CheckpointError {
    fn needs_merge(message: String) -> Self {
        Self {
            state: "needs-merge",
            message,
        }
    }
}

impl From<String> for CheckpointError {
    fn from(message: String) -> Self {
        Self {
            state: "rejected",
            message,
        }
    }
}

/// A held integration lane. Backed by an `flock`, so the OS releases it if the
/// holder crashes — the old `mkdir` lease could wedge a worker's lane forever.
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

    fn preserve(mut self, destination: &Path) -> Result<(), String> {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        fs::rename(&self.path, destination).map_err(|error| error.to_string())?;
        self.remove = false;
        Ok(())
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

struct ValidatedChanges {
    new_records: BTreeMap<PathBuf, FileData>,
    organization: Option<BTreeMap<PathBuf, FileData>>,
    changed_files: usize,
}

impl ValidatedChanges {
    fn is_empty(&self) -> bool {
        self.changed_files == 0
    }
}

pub fn provision(
    worker: &str,
    run_id: &str,
    requested_mode: &str,
    tainted: bool,
) -> Result<RunStorage, String> {
    let worker = short_worker(worker);
    safe_component(worker, "worker")?;
    safe_component(run_id, "run id")?;
    let mut mode = KnowledgeMode::parse(requested_mode)?;
    let policy = crate::worker::storage::load(worker);
    // The memory/knowledge boundary: a run that saw interaction content or
    // context never gets a writable mount under clean-room policy — the
    // provenance of the run decides, not the model.
    if tainted {
        if mode == KnowledgeMode::Reorganization {
            return Err("a knowledge reorganization cannot run with interaction context".into());
        }
        if policy.knowledge.write_from == "clean-room" {
            mode = KnowledgeMode::Read;
        }
    }
    let run_dir = paths::run_dir(run_id);

    if !policy.knowledge.enabled {
        if mode == KnowledgeMode::Reorganization {
            return Err("knowledge is disabled; reorganization cannot run".into());
        }
        crate::run::runlog::attach_storage(run_id, None)?;
        return Ok(RunStorage {
            worker: worker.into(),
            run_id: run_id.into(),
            knowledge: None,
            _reorganization_lease: None,
        });
    }

    let repo = ensure_repo(worker)?;
    let reorganization_lease = if mode == KnowledgeMode::Reorganization {
        Some(acquire_lease(&worker_dir(worker).join("reorganization.lock"))?)
    } else {
        None
    };
    // A reorganization snapshots main while holding the integration lane. All
    // already-running checkpoints drain before this point; later append jobs
    // may continue because their records/ write set is disjoint.
    let snapshot_lane = if mode == KnowledgeMode::Reorganization {
        Some(acquire_lease(&worker_dir(worker).join("integrate.lock"))?)
    } else {
        None
    };
    let base_commit = head(&repo)?;
    let path = run_dir.join("knowledge");
    if path.exists() {
        return Err(format!(
            "knowledge checkout already exists at {}",
            path.display()
        ));
    }
    clone_at(&repo, &path, &base_commit)?;
    drop(snapshot_lane);
    fs::remove_dir_all(path.join(".git")).map_err(|error| error.to_string())?;

    let record_namespace = format!("n_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
    let state = if mode == KnowledgeMode::Read {
        "read-only"
    } else {
        "active"
    };
    crate::run::runlog::attach_storage(
        run_id,
        Some(KnowledgeRunRecord {
            base_commit: base_commit.clone(),
            mode: mode.as_str().into(),
            record_namespace: record_namespace.clone(),
            state: state.into(),
            produced_commit: None,
            error: None,
        }),
    )?;
    if mode == KnowledgeMode::Reorganization {
        crate::worker::journal::append_required(
            &journal_worker(worker),
            run_id,
            "knowledge-reorganization-lease-acquired",
            json!({ "base_commit": base_commit }),
        )?;
    }
    Ok(RunStorage {
        worker: worker.into(),
        run_id: run_id.into(),
        knowledge: Some(Checkout {
            worker: worker.into(),
            run_id: run_id.into(),
            path,
            base_commit,
            record_namespace,
            knowledge_policy: policy.knowledge,
            mode,
        }),
        _reorganization_lease: reorganization_lease,
    })
}

pub fn checkpoint(checkout: &Checkout) -> Result<Checkpoint, String> {
    if checkout.mode == KnowledgeMode::Read {
        return Err("a read-only checkout does not checkpoint".into());
    }
    match checkpoint_inner(checkout) {
        Ok(result) => {
            crate::run::runlog::update_knowledge(
                &checkout.run_id,
                result.state,
                result.commit.as_deref(),
                None,
            )?;
            Ok(result)
        }
        Err(error) => {
            let _ = crate::run::runlog::update_knowledge(
                &checkout.run_id,
                error.state,
                None,
                Some(&error.message),
            );
            let _ = crate::worker::journal::append_required(
                &journal_worker(&checkout.worker),
                &checkout.run_id,
                "knowledge-checkpoint-rejected",
                json!({ "base_commit": checkout.base_commit, "state": error.state, "error": error.message }),
            );
            Err(error.message)
        }
    }
}

fn checkpoint_inner(checkout: &Checkout) -> Result<Checkpoint, CheckpointError> {
    let worker_dir = worker_dir(&checkout.worker);
    let repo = canonical_repo(&checkout.worker);
    let base = TempTree::new(&worker_dir, "base")?;
    clone_at(&repo, &base.path, &checkout.base_commit)?;
    let changes = match checkout.mode {
        // Unreachable: checkpoint() rejects read-only checkouts before here.
        KnowledgeMode::Read => {
            return Err(CheckpointError::from(
                "a read-only checkout does not checkpoint".to_string(),
            ))
        }
        KnowledgeMode::Append => {
            let new_records = validate_append(
                &base.path,
                &checkout.path,
                &checkout.record_namespace,
                &checkout.knowledge_policy,
            )?;
            ValidatedChanges {
                changed_files: new_records.len(),
                new_records,
                organization: None,
            }
        }
        KnowledgeMode::Reorganization => validate_reorganization(
            &base.path,
            &checkout.path,
            &checkout.record_namespace,
            &checkout.knowledge_policy,
        )?,
    };
    if changes.is_empty() {
        crate::worker::journal::append_required(
            &journal_worker(&checkout.worker),
            &checkout.run_id,
            "knowledge-checkpoint-empty",
            json!({ "base_commit": checkout.base_commit }),
        )?;
        return Ok(Checkpoint {
            state: "no-changes",
            commit: None,
            files: 0,
        });
    }

    // The boundary scan, defense-in-depth: knowledge describes the world,
    // never the run's own participants. Clean runs have no participants, so
    // this only bites in any-run mode or on mention syntax — the hard
    // guarantee is the read-only mount, not this scan.
    {
        let context = crate::worker::memory::load_run_context(&checkout.run_id);
        let markers = crate::worker::boundary::participant_markers(&context);
        let all_changed = changes
            .new_records
            .iter()
            .chain(changes.organization.iter().flatten());
        for (path, file) in all_changed {
            if let Ok(text) = std::str::from_utf8(&file.bytes) {
                if let Some(hit) = crate::worker::boundary::scan(text, &markers, false) {
                    return Err(CheckpointError::from(format!(
                        "{} references a conversation participant (\"{hit}\") — that belongs in memory, not in knowledge",
                        path.display()
                    )));
                }
            }
        }
    }

    let _lease = acquire_lease(&worker_dir.join("integrate.lock"))?;
    let latest = head(&repo)?;
    let integration = TempTree::new(&worker_dir, "integrate")?;
    clone_at(&repo, &integration.path, &latest)?;
    if let Some(organization) = changes.organization.as_ref() {
        let base_files = collect_files(&base.path, true)?;
        let latest_files = collect_files(&integration.path, true)?;
        let base_organization = files_under(&base_files, "organization");
        let latest_organization = files_under(&latest_files, "organization");
        if let Err(error) = durable_paths_unchanged(&base_files, &latest_files) {
            let pending = worker_dir
                .join("pending")
                .join(format!("{}-durable-path-changed", checkout.run_id));
            integration.preserve(&pending)?;
            return Err(CheckpointError::needs_merge(format!(
                "{error}; integration preserved at {}",
                pending.display()
            )));
        }
        if base_organization != latest_organization {
            let pending = worker_dir
                .join("pending")
                .join(format!("{}-organization-changed", checkout.run_id));
            integration.preserve(&pending)?;
            return Err(CheckpointError::needs_merge(format!(
                "organization changed after the reorganization snapshot; integration preserved at {}",
                pending.display()
            )));
        }
        let organization_dir = integration.path.join("organization");
        if organization_dir.exists() {
            fs::remove_dir_all(&organization_dir).map_err(|error| error.to_string())?;
        }
        fs::create_dir_all(&organization_dir).map_err(|error| error.to_string())?;
        for (relative, file) in organization {
            write_file(&integration.path, relative, file)?;
        }
    }
    for (relative, file) in &changes.new_records {
        let destination = integration.path.join(relative);
        if destination.exists() {
            let pending = worker_dir
                .join("pending")
                .join(format!("{}-path-collision", checkout.run_id));
            integration.preserve(&pending)?;
            return Err(CheckpointError::needs_merge(format!(
                "knowledge path collision at {}; integration preserved at {}",
                relative.display(),
                pending.display()
            )));
        }
        write_file(&integration.path, relative, file)?;
    }

    run_git(&integration.path, &["add", "--all"])?;
    run_git(
        &integration.path,
        &["config", "user.name", "Roster Knowledge"],
    )?;
    run_git(
        &integration.path,
        &["config", "user.email", "knowledge@roster.local"],
    )?;
    let record = crate::run::runlog::load(&checkout.run_id);
    let task = record
        .as_ref()
        .and_then(|record| record.task_id.as_deref())
        .unwrap_or("-");
    let context = crate::worker::memory::load_run_context(&checkout.run_id);
    let channel = context.channel_scope_id().unwrap_or_else(|| "-".into());
    let subject = if checkout.mode == KnowledgeMode::Reorganization {
        "Reorganize worker knowledge"
    } else {
        "Add worker knowledge"
    };
    let message = format!(
        "{} from run {}\n\nRoster-Worker: {}\nRoster-Run: {}\nRoster-Task: {}\nRoster-Channel: {}\nRoster-Base-Commit: {}\nRoster-Mode: {}",
        subject,
        checkout.run_id,
        checkout.worker,
        checkout.run_id,
        task,
        channel,
        checkout.base_commit,
        checkout.mode.as_str(),
    );
    run_git_owned(
        &integration.path,
        vec!["commit".into(), "-m".into(), message],
    )?;
    let commit = run_git(&integration.path, &["rev-parse", "HEAD"])?;

    crate::worker::journal::append_required(
        &journal_worker(&checkout.worker),
        &checkout.run_id,
        "knowledge-integration-started",
        json!({
            "base_commit": checkout.base_commit,
            "integration_base": latest,
            "candidate_commit": commit,
            "files": changes.changed_files,
            "mode": checkout.mode.as_str(),
        }),
    )?;
    let incoming_ref = format!("refs/roster/incoming/{}", checkout.run_id);
    if let Err(error) = run_git_owned(
        &integration.path,
        vec![
            "push".into(),
            "origin".into(),
            format!("HEAD:{incoming_ref}"),
        ],
    ) {
        let pending =
            worker_dir
                .join("pending")
                .join(format!("{}-{}", checkout.run_id, &commit[..12]));
        integration.preserve(&pending)?;
        return Err(CheckpointError::from(format!(
            "could not preserve incoming knowledge commit: {error}; candidate preserved at {}",
            pending.display()
        )));
    }
    if let Err(error) = run_git(
        &integration.path,
        &["push", "origin", "HEAD:refs/heads/main"],
    ) {
        let pending =
            worker_dir
                .join("pending")
                .join(format!("{}-{}", checkout.run_id, &commit[..12]));
        integration.preserve(&pending)?;
        return Err(CheckpointError::needs_merge(format!(
            "knowledge integration failed: {error}; candidate preserved at {}",
            pending.display()
        )));
    }
    let _ = run_git_owned(
        std::path::Path::new("."),
        vec![
            format!("--git-dir={}", repo.display()),
            "update-ref".into(),
            "-d".into(),
            incoming_ref,
        ],
    );
    crate::worker::journal::append_required(
        &journal_worker(&checkout.worker),
        &checkout.run_id,
        "knowledge-integrated",
        json!({
            "base_commit": checkout.base_commit,
            "commit": commit,
            "files": changes.changed_files,
            "mode": checkout.mode.as_str(),
        }),
    )?;
    Ok(Checkpoint {
        state: "integrated",
        commit: Some(commit),
        files: changes.changed_files,
    })
}

pub fn quarantine(checkout: &Checkout, reason: &str) {
    let _ =
        crate::run::runlog::update_knowledge(&checkout.run_id, "quarantined", None, Some(reason));
    let _ = crate::worker::journal::append_required(
        &journal_worker(&checkout.worker),
        &checkout.run_id,
        "knowledge-checkout-quarantined",
        json!({ "base_commit": checkout.base_commit, "path": checkout.path, "reason": reason }),
    );
}

pub fn release_reorganization(storage: &mut RunStorage) -> Result<(), String> {
    if storage._reorganization_lease.is_none() {
        return Ok(());
    }
    let journal_result = crate::worker::journal::append_required(
        &journal_worker(&storage.worker),
        &storage.run_id,
        "knowledge-reorganization-lease-released",
        json!({}),
    );
    drop(storage._reorganization_lease.take());
    journal_result
}

fn validate_append(
    base: &Path,
    checkout: &Path,
    namespace: &str,
    policy: &KnowledgePolicy,
) -> Result<BTreeMap<PathBuf, FileData>, String> {
    let base_files = collect_files(base, true)?;
    let current_files = collect_files(checkout, false)?;
    let total: u64 = current_files
        .values()
        .map(|file| file.bytes.len() as u64)
        .sum();
    if total > policy.max_repo_bytes {
        return Err(format!(
            "knowledge checkout is {total} bytes, over the {} byte limit",
            policy.max_repo_bytes
        ));
    }
    for (path, original) in &base_files {
        let Some(current) = current_files.get(path) else {
            return Err(format!("append mode cannot delete {}", path.display()));
        };
        if current.bytes != original.bytes {
            return Err(format!("append mode cannot modify {}", path.display()));
        }
    }

    let mut additions = BTreeMap::new();
    for (path, file) in current_files {
        if base_files.contains_key(&path) {
            continue;
        }
        validate_new_record(&path, &file.bytes, namespace, policy)?;
        additions.insert(path, file);
    }
    Ok(additions)
}

fn validate_reorganization(
    base: &Path,
    checkout: &Path,
    namespace: &str,
    policy: &KnowledgePolicy,
) -> Result<ValidatedChanges, String> {
    let base_files = collect_files(base, true)?;
    let current_files = collect_files(checkout, false)?;
    enforce_repo_size(&current_files, policy)?;

    durable_paths_unchanged(&base_files, &current_files)?;

    let mut new_records = BTreeMap::new();
    for (path, file) in &current_files {
        if is_under(path, "organization") {
            validate_organization_file(path, &file.bytes, policy)?;
        } else if is_under(path, "records") {
            if !base_files.contains_key(path) {
                validate_new_record(path, &file.bytes, namespace, policy)?;
                new_records.insert(path.clone(), file.clone());
            }
        } else if !base_files.contains_key(path) {
            return Err(format!(
                "reorganization may add files only under records/ or organization/: {}",
                path.display()
            ));
        }
    }

    let base_organization = files_under(&base_files, "organization");
    let organization = files_under(&current_files, "organization");
    let organization_changes = changed_file_count(&base_organization, &organization);
    Ok(ValidatedChanges {
        changed_files: new_records.len() + organization_changes,
        new_records,
        organization: Some(organization),
    })
}

fn durable_paths_unchanged(
    base: &BTreeMap<PathBuf, FileData>,
    current: &BTreeMap<PathBuf, FileData>,
) -> Result<(), String> {
    for (path, original) in base {
        if is_under(path, "organization") {
            continue;
        }
        let Some(value) = current.get(path) else {
            return Err(format!(
                "reorganization cannot delete durable path {}",
                path.display()
            ));
        };
        if value != original {
            return Err(format!(
                "reorganization cannot modify durable path {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn enforce_repo_size(
    files: &BTreeMap<PathBuf, FileData>,
    policy: &KnowledgePolicy,
) -> Result<(), String> {
    let total: u64 = files.values().map(|file| file.bytes.len() as u64).sum();
    if total > policy.max_repo_bytes {
        return Err(format!(
            "knowledge checkout is {total} bytes, over the {} byte limit",
            policy.max_repo_bytes
        ));
    }
    Ok(())
}

fn is_under(path: &Path, domain: &str) -> bool {
    path.components().next() == Some(Component::Normal(domain.as_ref()))
}

fn files_under(files: &BTreeMap<PathBuf, FileData>, domain: &str) -> BTreeMap<PathBuf, FileData> {
    files
        .iter()
        .filter(|(path, _)| is_under(path, domain))
        .map(|(path, file)| (path.clone(), file.clone()))
        .collect()
}

fn changed_file_count(
    before: &BTreeMap<PathBuf, FileData>,
    after: &BTreeMap<PathBuf, FileData>,
) -> usize {
    before
        .keys()
        .chain(after.keys())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|path| before.get(path) != after.get(path))
        .count()
}

fn write_file(root: &Path, relative: &Path, file: &FileData) -> Result<(), String> {
    let destination = root.join(relative);
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    fs::write(destination, &file.bytes).map_err(|error| error.to_string())
}

fn collect_files(root: &Path, allow_git: bool) -> Result<BTreeMap<PathBuf, FileData>, String> {
    fn walk(
        root: &Path,
        dir: &Path,
        allow_git: bool,
        out: &mut BTreeMap<PathBuf, FileData>,
    ) -> Result<(), String> {
        for entry in fs::read_dir(dir).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .map_err(|error| error.to_string())?
                .to_path_buf();
            if allow_git && relative.components().next() == Some(Component::Normal(".git".as_ref()))
            {
                continue;
            }
            let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "knowledge cannot contain symlink {}",
                    relative.display()
                ));
            }
            if metadata.is_dir() {
                walk(root, &path, allow_git, out)?;
            } else if metadata.is_file() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::{MetadataExt, PermissionsExt};
                    if metadata.permissions().mode() & 0o111 != 0 {
                        return Err(format!(
                            "knowledge file is executable: {}",
                            relative.display()
                        ));
                    }
                    if metadata.nlink() > 1 {
                        return Err(format!(
                            "knowledge file is hard-linked: {}",
                            relative.display()
                        ));
                    }
                }
                out.insert(
                    relative,
                    FileData {
                        bytes: fs::read(&path).map_err(|error| error.to_string())?,
                    },
                );
            } else {
                return Err(format!(
                    "unsupported knowledge file type: {}",
                    relative.display()
                ));
            }
        }
        Ok(())
    }

    let mut out = BTreeMap::new();
    walk(root, root, allow_git, &mut out)?;
    Ok(out)
}

fn validate_new_record(
    path: &Path,
    bytes: &[u8],
    namespace: &str,
    policy: &KnowledgePolicy,
) -> Result<(), String> {
    validate_relative_path(path)?;
    if path.components().next() != Some(Component::Normal("records".as_ref()))
        || path.components().count() < 2
    {
        return Err(format!(
            "append mode may add files only under records/: {}",
            path.display()
        ));
    }
    validate_text_file(path, bytes, policy)?;
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    let Some((slug, id)) = stem.rsplit_once("--") else {
        return Err(format!(
            "record filename must end in --{namespace}_<number>: {}",
            path.display()
        ));
    };
    let sequence = id
        .strip_prefix(namespace)
        .and_then(|value| value.strip_prefix('_'))
        .unwrap_or("");
    if slug.is_empty() || sequence.is_empty() || !sequence.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(format!(
            "record filename must end in --{namespace}_<number>: {}",
            path.display()
        ));
    }
    Ok(())
}

fn validate_organization_file(
    path: &Path,
    bytes: &[u8],
    policy: &KnowledgePolicy,
) -> Result<(), String> {
    validate_relative_path(path)?;
    if !is_under(path, "organization") || path.components().count() < 2 {
        return Err(format!(
            "reorganization files must remain under organization/: {}",
            path.display()
        ));
    }
    validate_text_file(path, bytes, policy)
}

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

fn ensure_repo(worker: &str) -> Result<PathBuf, String> {
    let dir = worker_dir(worker);
    fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let _lease = acquire_lease(&dir.join("integrate.lock"))?;
    let repo = canonical_repo(worker);
    if repo.join("refs/heads/main").exists() || head(&repo).is_ok() {
        return Ok(repo);
    }
    if repo.exists() {
        return Err(format!(
            "knowledge repository exists without main: {}",
            repo.display()
        ));
    }
    run_git_owned(
        std::path::Path::new("."),
        vec![
            "init".into(),
            "--bare".into(),
            "--initial-branch=main".into(),
            repo.display().to_string(),
        ],
    )?;
    let init = TempTree::new(&dir, "init")?;
    clone_repo(&repo, &init.path)?;
    fs::create_dir_all(init.path.join("records")).map_err(|error| error.to_string())?;
    fs::create_dir_all(init.path.join("organization")).map_err(|error| error.to_string())?;
    fs::write(
        init.path.join("records/README.md"),
        "# Durable records\n\nAppend runs add uniquely named research notes here. Use the record namespace supplied in `ROSTER_RECORD_NAMESPACE`.\n",
    )
    .map_err(|error| error.to_string())?;
    fs::write(
        init.path.join("organization/README.md"),
        "# Knowledge organization\n\nThis mutable view is maintained by an exclusive reorganization job. Append runs must not edit it.\n",
    )
    .map_err(|error| error.to_string())?;
    run_git(&init.path, &["config", "user.name", "Roster Knowledge"])?;
    run_git(
        &init.path,
        &["config", "user.email", "knowledge@roster.local"],
    )?;
    run_git(&init.path, &["add", "--all"])?;
    run_git(
        &init.path,
        &["commit", "-m", "Initialize worker knowledge repository"],
    )?;
    run_git(&init.path, &["push", "origin", "main"])?;
    Ok(repo)
}

fn acquire_lease(path: &Path) -> Result<Lease, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    // Migration: the previous scheme made this path a *directory* (holding an
    // `owner` file). flock needs a regular file, so clear a stale leftover — it
    // can only exist if an old binary crashed mid-lease, which is exactly the
    // wedge this change removes.
    if path.is_dir() {
        let _ = fs::remove_dir_all(path);
    }
    // Bounded wait (~5s), same as before, but on a lock the OS frees on crash.
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
        std::path::Path::new("."),
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

fn head(repo: &Path) -> Result<String, String> {
    run_git_owned(
        std::path::Path::new("."),
        vec![
            format!("--git-dir={}", repo.display()),
            "rev-parse".into(),
            "refs/heads/main".into(),
        ],
    )
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
    head(&repo)
}

pub fn repo_path(worker: &str) -> Result<PathBuf, String> {
    let worker = short_worker(worker);
    safe_component(worker, "worker")?;
    let repo = canonical_repo(worker);
    if !repo.exists() {
        return Err(format!(
            "knowledge repository for {worker} is not initialized; create the worker first: roster worker init {worker}"
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
            max_repo_bytes: 10_000,
            ..Default::default()
        }
    }

    #[test]
    fn append_validation_accepts_only_namespaced_additions() {
        let temp = std::env::temp_dir().join(format!("roster-knowledge-{}", uuid::Uuid::new_v4()));
        let base = temp.join("base");
        let checkout = temp.join("checkout");
        fs::create_dir_all(base.join("records")).unwrap();
        fs::create_dir_all(checkout.join("records")).unwrap();
        fs::write(base.join("records/README.md"), "base").unwrap();
        fs::write(checkout.join("records/README.md"), "base").unwrap();
        fs::write(
            checkout.join("records/example--n_abc123_01.md"),
            "# Example\n",
        )
        .unwrap();
        let additions = validate_append(&base, &checkout, "n_abc123", &policy()).unwrap();
        assert_eq!(additions.len(), 1);

        fs::write(checkout.join("records/README.md"), "changed").unwrap();
        assert!(validate_append(&base, &checkout, "n_abc123", &policy()).is_err());
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn append_validation_rejects_organization_and_wrong_namespace() {
        let p = policy();
        assert!(
            validate_new_record(Path::new("organization/topic.md"), b"text", "n_abc", &p).is_err()
        );
        assert!(validate_new_record(
            Path::new("records/topic--n_other_01.md"),
            b"text",
            "n_abc",
            &p
        )
        .is_err());
    }

    #[test]
    fn reorganization_changes_only_organization_and_new_records() {
        let temp =
            std::env::temp_dir().join(format!("roster-reorganization-{}", uuid::Uuid::new_v4()));
        let base = temp.join("base");
        let checkout = temp.join("checkout");
        for root in [&base, &checkout] {
            fs::create_dir_all(root.join("records")).unwrap();
            fs::create_dir_all(root.join("organization")).unwrap();
            fs::write(root.join("records/source--n_old_01.md"), "source").unwrap();
            fs::write(root.join("organization/topic.md"), "old pointer").unwrap();
        }
        fs::write(checkout.join("organization/topic.md"), "new pointer").unwrap();
        fs::write(
            checkout.join("records/synthesis--n_run123_01.md"),
            "synthesis",
        )
        .unwrap();

        let changes = validate_reorganization(&base, &checkout, "n_run123", &policy()).unwrap();
        assert_eq!(changes.new_records.len(), 1);
        assert_eq!(changes.changed_files, 2);

        fs::write(checkout.join("records/source--n_old_01.md"), "rewritten").unwrap();
        assert!(validate_reorganization(&base, &checkout, "n_run123", &policy()).is_err());
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn organization_change_count_includes_deletions() {
        let before = BTreeMap::from([
            (
                PathBuf::from("organization/a.md"),
                FileData {
                    bytes: b"a".to_vec(),
                },
            ),
            (
                PathBuf::from("organization/b.md"),
                FileData {
                    bytes: b"b".to_vec(),
                },
            ),
        ]);
        let after = BTreeMap::from([
            (
                PathBuf::from("organization/a.md"),
                FileData {
                    bytes: b"changed".to_vec(),
                },
            ),
            (
                PathBuf::from("organization/c.md"),
                FileData {
                    bytes: b"c".to_vec(),
                },
            ),
        ]);
        assert_eq!(changed_file_count(&before, &after), 3);
    }
}
