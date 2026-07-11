//! Git-backed world knowledge for a worker. The box receives a plain checkout
//! with no Git metadata. The trusted host validates additions and creates the
//! commit in a serialized per-worker integration lane.

use crate::runlog::KnowledgeRunRecord;
use crate::storage::{KnowledgePolicy, ScratchPolicy};
use crate::util::root;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

const KNOWLEDGE_MOUNT: &str = "/opt/roster/knowledge";
const SCRATCH_MOUNT: &str = "/opt/roster/scratch";
const ALLOWED_EXTENSIONS: &[&str] = &["md", "json", "jsonl", "yaml", "yml", "csv", "txt"];

#[derive(Debug, Clone)]
pub struct Checkout {
    pub worker: String,
    pub run_id: String,
    pub path: PathBuf,
    pub base_commit: String,
    pub record_namespace: String,
    pub knowledge_policy: KnowledgePolicy,
}

impl Checkout {
    pub fn knowledge_mount(&self) -> &'static str {
        KNOWLEDGE_MOUNT
    }
}

#[derive(Debug, Clone)]
pub struct RunStorage {
    pub worker: String,
    pub run_id: String,
    pub scratch: PathBuf,
    pub scratch_policy: ScratchPolicy,
    pub knowledge: Option<Checkout>,
}

impl RunStorage {
    pub fn scratch_mount(&self) -> &'static str {
        SCRATCH_MOUNT
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

struct Lease {
    path: PathBuf,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
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

#[derive(Debug)]
struct FileData {
    bytes: Vec<u8>,
}

pub fn provision(worker: &str, run_id: &str) -> Result<RunStorage, String> {
    let worker = short_worker(worker);
    safe_component(worker, "worker")?;
    safe_component(run_id, "run id")?;
    let policy = crate::storage::load(worker);
    let run_dir = root().join("runs").join(run_id);
    let scratch = run_dir.join("scratch");
    fs::create_dir_all(&scratch).map_err(|error| error.to_string())?;

    if !policy.knowledge.enabled {
        crate::runlog::attach_storage(run_id, None)?;
        return Ok(RunStorage {
            worker: worker.into(),
            run_id: run_id.into(),
            scratch,
            scratch_policy: policy.scratch,
            knowledge: None,
        });
    }

    let repo = ensure_repo(worker)?;
    let base_commit = head(&repo)?;
    let path = run_dir.join("knowledge");
    if path.exists() {
        return Err(format!(
            "knowledge checkout already exists at {}",
            path.display()
        ));
    }
    clone_at(&repo, &path, &base_commit)?;
    fs::remove_dir_all(path.join(".git")).map_err(|error| error.to_string())?;

    let record_namespace = format!("n_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
    crate::runlog::attach_storage(
        run_id,
        Some(KnowledgeRunRecord {
            base_commit: base_commit.clone(),
            mode: "append".into(),
            record_namespace: record_namespace.clone(),
            state: "active".into(),
            produced_commit: None,
            error: None,
        }),
    )?;
    Ok(RunStorage {
        worker: worker.into(),
        run_id: run_id.into(),
        scratch,
        scratch_policy: policy.scratch,
        knowledge: Some(Checkout {
            worker: worker.into(),
            run_id: run_id.into(),
            path,
            base_commit,
            record_namespace,
            knowledge_policy: policy.knowledge,
        }),
    })
}

pub fn checkpoint(checkout: &Checkout) -> Result<Checkpoint, String> {
    match checkpoint_inner(checkout) {
        Ok(result) => {
            crate::runlog::update_knowledge(
                &checkout.run_id,
                result.state,
                result.commit.as_deref(),
                None,
            )?;
            Ok(result)
        }
        Err(error) => {
            let _ = crate::runlog::update_knowledge(
                &checkout.run_id,
                error.state,
                None,
                Some(&error.message),
            );
            let _ = crate::journal::append_required(
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
    let additions = validate_append(
        &base.path,
        &checkout.path,
        &checkout.record_namespace,
        &checkout.knowledge_policy,
    )?;
    if additions.is_empty() {
        crate::journal::append_required(
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

    let _lease = acquire_lease(&worker_dir.join("integrate.lock"))?;
    let latest = head(&repo)?;
    let integration = TempTree::new(&worker_dir, "integrate")?;
    clone_at(&repo, &integration.path, &latest)?;
    for (relative, file) in &additions {
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
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        fs::write(&destination, &file.bytes).map_err(|error| error.to_string())?;
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
    let record = crate::runlog::load(&checkout.run_id);
    let task = record
        .as_ref()
        .and_then(|record| record.task_id.as_deref())
        .unwrap_or("-");
    let context = crate::memory::load_run_context(&checkout.run_id);
    let channel = context.channel_scope_id().unwrap_or_else(|| "-".into());
    let message = format!(
        "Add knowledge from run {}\n\nRoster-Worker: {}\nRoster-Run: {}\nRoster-Task: {}\nRoster-Channel: {}\nRoster-Base-Commit: {}\nRoster-Mode: append",
        checkout.run_id,
        checkout.worker,
        checkout.run_id,
        task,
        channel,
        checkout.base_commit,
    );
    run_git_owned(
        &integration.path,
        vec!["commit".into(), "-m".into(), message],
    )?;
    let commit = run_git(&integration.path, &["rev-parse", "HEAD"])?;

    crate::journal::append_required(
        &journal_worker(&checkout.worker),
        &checkout.run_id,
        "knowledge-integration-started",
        json!({
            "base_commit": checkout.base_commit,
            "integration_base": latest,
            "candidate_commit": commit,
            "files": additions.len(),
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
        &root(),
        vec![
            format!("--git-dir={}", repo.display()),
            "update-ref".into(),
            "-d".into(),
            incoming_ref,
        ],
    );
    crate::journal::append_required(
        &journal_worker(&checkout.worker),
        &checkout.run_id,
        "knowledge-integrated",
        json!({
            "base_commit": checkout.base_commit,
            "commit": commit,
            "files": additions.len(),
        }),
    )?;
    Ok(Checkpoint {
        state: "integrated",
        commit: Some(commit),
        files: additions.len(),
    })
}

pub fn quarantine(checkout: &Checkout, reason: &str) {
    let _ = crate::runlog::update_knowledge(&checkout.run_id, "quarantined", None, Some(reason));
    let _ = crate::journal::append_required(
        &journal_worker(&checkout.worker),
        &checkout.run_id,
        "knowledge-checkout-quarantined",
        json!({ "base_commit": checkout.base_commit, "path": checkout.path, "reason": reason }),
    );
}

pub fn cleanup_scratch(storage: &RunStorage, crashed: bool) -> Result<(), String> {
    let cleanup = if crashed {
        storage.scratch_policy.cleanup_on_crash
    } else {
        storage.scratch_policy.cleanup_on_exit
    };
    if !cleanup {
        crate::runlog::update_scratch(&storage.run_id, "preserved", None)?;
        return Ok(());
    }
    match fs::remove_dir_all(&storage.scratch) {
        Ok(()) => {
            crate::runlog::update_scratch(&storage.run_id, "cleaned", None)?;
            let _ = crate::journal::append_required(
                &journal_worker(&storage.worker),
                &storage.run_id,
                "scratch-cleaned",
                json!({ "crashed": crashed }),
            );
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            crate::runlog::update_scratch(&storage.run_id, "cleaned", None)
        }
        Err(error) => {
            let message = error.to_string();
            let _ =
                crate::runlog::update_scratch(&storage.run_id, "cleanup-failed", Some(&message));
            Err(message)
        }
    }
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
        &root(),
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
    for _ in 0..250 {
        match fs::create_dir(path) {
            Ok(()) => {
                let _ = fs::write(path.join("owner"), format!("pid={}\n", std::process::id()));
                return Ok(Lease { path: path.into() });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                thread::sleep(Duration::from_millis(20));
            }
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
        &root(),
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
        &root(),
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
    root().join("knowledge").join(short_worker(worker))
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
    Ok(head(&repo)?)
}

pub fn repo_path(worker: &str) -> Result<PathBuf, String> {
    let worker = short_worker(worker);
    safe_component(worker, "worker")?;
    let repo = canonical_repo(worker);
    if !repo.exists() {
        return Err(format!(
            "knowledge repository for {worker} is not initialized; create or deploy the worker first"
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
}
