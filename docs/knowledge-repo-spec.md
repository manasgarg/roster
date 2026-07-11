# Worker knowledge repository, scratch space, and publishing (spec)

**Status: proposed — not yet implemented.** This document defines how a worker
researches and organizes knowledge without mixing research notes with interaction
memory. It also defines the concurrency model for multiple runs of the same
worker.

## Outcome

Each worker receives three work surfaces:

1. **Knowledge repository** — one private Git repository per worker. The worker
   organizes its durable notes about the world here.
2. **Scratch space** — one private temporary directory per run or warm session.
   Downloads and intermediate files go here and are deleted when the run exits.
3. **Publication blob store** — durable immutable or versioned documents that
   the worker intentionally publishes through a governed action.

Interaction memory remains a fourth, separate system. It contains continuity
about the worker, channels, and users. Research sources, notes, and synthesized
documents never enter interaction memory merely because the worker created or
read them.

The journal records activity and durable pointers. Git records the history of
how the worker organized knowledge.

## Boundary between the systems

| System | Contains | Does not contain |
|---|---|---|
| Interaction memory | User preferences, channel decisions, interaction context | Research findings, downloaded sources, briefs |
| Knowledge repository | Structured notes, claims, source references, topic organization, internal syntheses | User-memory records, credentials, runtime enforcement state |
| Scratch space | Downloads, extraction output, temporary datasets, draft files | Anything expected to survive the run |
| Blob store | Intentionally published documents and their versions | General scratch files or a hidden research-memory layer |
| Journal | Fetch, integration, publication, failure, and gate events with pointers | Duplicated note or document bodies |

The knowledge repository is accessible data, not an automatically injected
prompt block. A worker may search and read it when relevant. The context compiler
records which Git commit was mounted, but it does not render the entire repository
as Memory.

## One repository per worker

The host owns a canonical Git repository for every worker:

```text
knowledge/<worker>/repo.git     canonical host-side repository
```

The canonical branch is `main`. Git metadata, hooks, branch refs, and integration
credentials remain host-controlled and are never writable from inside a box.

The repository separates durable records from their organization:

```text
records/                    uniquely named durable notes and syntheses
organization/               the worker's mutable, browsable view of its knowledge
```

The names are illustrative, but these two stable write domains are required.
Files in `records/` receive host-allocated, collision-free IDs. Their
platform-managed storage layout and the top-level domains are never reorganized
during normal operation. The worker controls the prose and metadata inside its
records and the internal shape of `organization/`, including:

- directory hierarchy;
- links, tags, and topic structure;
- indexes and pointers to internal syntheses;
- decisions about when material should be split or consolidated.

The platform imposes only the constraints required for safe concurrent updates,
filesystem safety, attribution, and resource limits.

The repository is not partitioned by channel. A channel is provenance for a
piece of work, not an ownership boundary for world knowledge. The worker may
choose channel-oriented folders when that is useful, but the platform does not
create a permanent branch, sub-repository, or note namespace per channel.

## Per-run checkout

At dispatch, the trusted host records the current integrated commit and creates a
private checkout for the run:

```text
runs/<run-id>/knowledge/       writable checkout at base commit
runs/<run-id>/scratch/         writable temporary storage
```

The knowledge checkout is mounted as data at a fixed path such as
`/opt/roster/knowledge`. It is not pi's working directory and is not searched for
instructions, skills, extensions, hooks, or settings.

The run record contains:

```json
{
  "knowledge": {
    "base_commit": "abc123...",
    "mode": "append"
  }
}
```

Concurrent runs may start from the same base commit. They never share a writable
checkout.

A run sees a pinned snapshot. New commits from other runs do not appear in its
checkout midway through model execution. After a successful checkpoint, the host
may refresh the checkout onto the newly integrated head at a defined tool or turn
boundary. The refresh must never happen while the model is reading or writing a
file.

## Two write modes

Arbitrary concurrent edits to the same files cannot be made conflict-free. The
repository therefore separates the paths written by ordinary research from the
paths written by reorganization. Only one reorganization job may exist at a
time, but it does not need to stop ordinary research jobs.

### Append mode

Append mode is the default for ordinary concurrent work.

A run may:

- create uniquely named files under `records/`;
- freely revise files that it created during the same run;
- link new notes to existing note IDs;
- add a correction or superseding note as a new file;
- create a new version of a synthesis as a new file.

A run may not:

- modify or delete a file that existed at its base commit;
- rename existing paths;
- write under `organization/`;
- rewrite shared indexes or canonical summaries in place;
- change repository configuration.

The host validates this rule from the Git diff before committing. A violating
diff is not integrated and remains available for repair.

New record paths must be collision-free. The host allocates their stable IDs;
the worker may include a human-readable slug:

```text
records/notes/prompt-caching--n_01JABC.md
records/notes/sso-support--n_01JDEF.md
```

The unique ID prevents two runs from creating the same path even when they
choose the same title. Tags, links, and other useful organization cues may live
in the record itself. A reorganization job examines records added since its last
pass and decides how to incorporate them into the organizational view.

### Reorganization mode

Reorganization mode gives one run an exclusive lease over `organization/` for
the worker's knowledge repository. Before granting the lease, the integrator
drains commits already queued for that worker so the job starts from the latest
completed work.

A reorganization run may:

- modify, rename, or delete paths under `organization/`;
- add new uniquely named records when consolidation produces a new synthesis;
- have a new record declare that it supersedes an older record, then update the
  organizational pointer;
- reorganize the browsable directory hierarchy;
- update canonical pointers to syntheses;
- rebuild hand-maintained indexes;
- perform repository-wide cleanup.

Routine reorganization does not rewrite or delete durable records. Consolidation
creates a new record and changes organizational pointers, preserving stable IDs
and links from runs that started on an older snapshot. Physical deletion is a
separate owner-only retention or emergency-purge operation.

While the lease is held, append-mode research runs continue normally. Their
commits add unique paths under `records/`; the reorganization job changes paths
under `organization/`. The two write sets therefore do not overlap. When either
commit rebases across the other, Git has no shared path to conflict on. Records
that were not visible to the reorganization job remain unorganized until its
next pass, but are still durable and directly searchable by stable ID and
metadata.

This is repository reorganization, not memory promotion. It changes organization
inside the worker's knowledge repository and does not change identity, purpose,
permissions, or interaction memory.

## Minimal note identity

The platform does not prescribe a domain ontology. For merge safety and durable
links, every durable note should have a stable ID. Markdown notes may use minimal
frontmatter:

```yaml
---
id: n_01JABC
created_at: 2026-07-11T12:00:00Z
sources:
  - fetch_01JXYZ
---
```

Everything else is worker-controlled. A worker may add titles, tags, confidence,
links, or custom fields when useful.

Links should prefer stable note IDs over paths so reorganization runs can change
the directory tree without breaking meaning. A deterministic ID index may be
rebuilt from frontmatter; it should not become a shared file that every append
run edits.

In append mode, a correction can be represented as another note:

```yaml
---
id: n_01JGHI
supersedes:
  - n_01JABC
---
```

A later reorganization run can consolidate the two if appropriate.

## Repository content policy

The repository is intended for durable, reviewable knowledge. Recommended
version-one file types are UTF-8 Markdown, JSON, JSONL, YAML, CSV, and other small
text formats.

Large binaries, fetched pages, PDFs, archives, and temporary datasets belong in
scratch. A document intended for publication belongs in the blob store after a
policy decision.

Before committing, the trusted host validates at least:

- every path remains inside the checkout;
- there are no symlinks, hard-link escapes, device files, sockets, or FIFOs;
- reserved control paths such as `.git`, Git hooks, pi configuration, skills,
  and runtime policy files are absent;
- file modes are non-executable unless explicitly permitted by policy;
- individual file and total repository limits are respected;
- obvious credentials and secrets are rejected;
- append-mode diffs contain only host-allocated additions under `records/`;
- reorganization-mode diffs do not rewrite durable records or reserved
  top-level domains.

Notes are untrusted advisory material. Reading a note may influence model
behavior, but it never grants capabilities or bypasses the gateway.

## Host-controlled commits

The worker edits files but does not operate the canonical Git repository. On an
explicit checkpoint or clean run exit, the trusted host:

1. validates the checkout and mode rules;
2. computes the file diff against the recorded base commit;
3. creates a commit on a run-specific integration ref;
4. adds trusted provenance trailers;
5. queues the commit in the worker's single integration lane.

Example commit metadata:

```text
Organize notes from vendor SSO research

Roster-Worker: yuko
Roster-Run: 2026-07-11-12-00-00-abcd
Roster-Task: t-a13f8c2d
Roster-Channel: discord:456
Roster-Base-Commit: abc123...
Roster-Mode: append
```

The model may suggest the commit subject. Trusted trailers and authorship are
host-generated.

An empty diff produces no commit. A failed validation produces a journal event
and does not mutate `main`.

## Serialized integration lane

Each worker has exactly one host-side knowledge integration lane.

For each queued commit, the integrator:

1. loads the latest `main`;
2. rebases the run commit onto it;
3. re-runs repository validation;
4. fast-forwards `main` atomically when clean;
5. records the final commit in the run and journal.

Concurrent append-mode runs touch disjoint host-allocated paths and therefore
rebase cleanly:

```text
main at A
  ├── run 1: A → B
  └── run 2: A → C

integrate B: main A → B
rebase C:    A → C becomes B → C'
integrate:   main B → C'
```

The same property holds when an append commit crosses a reorganization commit:
append mode adds unique `records/` paths, while the sole reorganization job owns
`organization/`. Any records created by the reorganization job also receive
unique host-allocated IDs. Therefore every valid commit rebases without a path
conflict as long as the single-reorganization lease holds.

A Git conflict among valid commits indicates a broken invariant, a software
defect, or repository history created outside this protocol. The system never
chooses a side or drops content. It:

- preserves the incoming commit/ref;
- leaves `main` unchanged for that item;
- records the conflicting paths and commits;
- marks the knowledge update `needs-merge`;
- queues or requests an explicit owner repair.

The default is no automatic model-authored conflict resolution. That can be
added later as a restricted repair workflow once its review semantics are
settled.

## Checkpoints, exits, and crashes

A long-running worker may request a notes checkpoint. Each successful checkpoint
captures all valid changes since the previous checkpoint, integrates them, and
updates the run checkout to the resulting base.

A clean exit performs one final automatic checkpoint before scratch cleanup.

On an abnormal exit:

- partial knowledge changes are not automatically integrated;
- the host preserves a diff or quarantined checkout for owner inspection;
- the journal records the recovery pointer;
- scratch is still cleaned according to the configured crash-cleanup policy.

This avoids committing half-written notes merely because a process crashed.

## Scratch space

Scratch is private to one run or warm session:

```text
runs/<run-id>/scratch/
```

It is used for:

- downloaded source material;
- extraction and conversion output;
- temporary datasets;
- intermediate drafts;
- generated files awaiting publication;
- any work that is not yet durable knowledge.

Scratch is never shared between concurrent runs. It has hard byte and file-count
limits. The host deletes it after the final notes checkpoint and all in-flight
publish operations have reached a terminal state.

No component should treat a scratch path as a durable reference. Journal entries
may include a scratch pointer, but must label it transient and include the run ID
and cleanup state.

## Download receipts

Governed download and fetch tools record an append-only journal event. The event
describes what was acquired without copying the full body into the journal:

```json
{
  "kind": "fetch-completed",
  "id": "fetch_01JXYZ",
  "ts": "2026-07-11T12:03:00Z",
  "worker": "yuko",
  "run_id": "2026-07-11-12-00-00-abcd",
  "source": "https://example.com/report.pdf",
  "media_type": "application/pdf",
  "bytes": 48122,
  "sha256": "...",
  "scratch": {
    "path": "downloads/report.pdf",
    "durability": "transient"
  }
}
```

Notes cite the durable receipt ID, source URL, retrieval time, and content hash.
Once scratch is deleted, those fields identify what was observed but do not imply
that the bytes remain retrievable.

For a download, the journal's content pointer is therefore the original source
locator plus the hash of the exact downloaded bytes. The scratch path is only an
operational pointer. It must never be presented as a durable content pointer.

Whether Roster also needs a private persistent source archive is an open decision
below. The publication store must not be used implicitly as a source archive.

## Publication blob store

Publishing is an explicit governed action. The worker chooses a file from its
scratch space or knowledge checkout and proposes publication metadata:

```text
publish_blob(path, logical_name, media_type, visibility, rationale)
```

The trusted host derives worker and run identity, verifies that the source path
is inside an authorized mount, and evaluates policy. Policy may consider:

- visibility and destination;
- media type and extension;
- size;
- channel or task provenance;
- sensitive-data and secret scanning;
- whether human approval is required.

On approval, the host copies the exact reviewed bytes into the blob store. Blobs
are immutable and content-addressed. Publishing a new version creates a new blob
rather than overwriting old bytes.

```json
{
  "kind": "publish-completed",
  "ts": "2026-07-11T12:30:00Z",
  "worker": "yuko",
  "run_id": "2026-07-11-12-00-00-abcd",
  "blob_id": "blob_01JPUB",
  "sha256": "...",
  "bytes": 91234,
  "media_type": "application/pdf",
  "logical_name": "vendor-sso-report",
  "version": 3,
  "visibility": "public",
  "url": "https://...",
  "knowledge_commit": "def456...",
  "note_ids": ["n_01JABC", "n_01JDEF"]
}
```

Mutable logical names or `latest` aliases are metadata pointers, not mutable blob
contents. Updating an alias is a separate governed operation.

After publishing, the worker may add a note that links to the blob. Publication
remains durable even if the later notes commit fails; the journal is the recovery
path.

## Journal and Git responsibilities

The journal records activities and outcomes:

- fetch requested/completed/failed;
- notes checkpoint validated/rejected;
- notes commit integrated/needs-merge;
- reorganization lease acquired/released;
- publish proposed/gated/completed/failed;
- scratch cleaned or quarantined.

Git records knowledge organization:

- files added, consolidated, moved, or removed;
- note and synthesis history;
- the run/task/channel provenance of each integration;
- one-command inspection and reversion of organizational changes.

The journal points to Git commits and blobs. It does not duplicate their content.
Git notes may point to fetch receipt IDs and published blobs.

The resulting lineage is:

```text
fetch receipt
  → knowledge note commit
    → synthesized document
      → governed published blob
```

## Interaction memory remains separate

The knowledge checkout is never read by the interaction-memory selector. A note
about a vendor, scientific result, or downloaded document cannot appear in the
Memory block merely because it exists in Git.

Likewise, worker/channel/user memories are not written into the knowledge
repository automatically. If a user explicitly asks for a durable research note
or publication, that is handled as knowledge work, not as a memory write.

The current implementation uses `notes/<worker>.jsonl` and `roster notes` for
interaction memory. Before exposing the Git-backed notes repository, the product
should remove this naming collision. The recommended migration is:

```text
notes/<worker>.jsonl  → memory/<worker>.jsonl
roster notes ...      → roster memory ...
```

The migration is a naming and storage move only; it does not convert interaction
memory into knowledge notes.

## Configuration

Recommended owner-controlled settings:

```toml
[knowledge]
enabled = true
normal_mode = "append"
max_file_chars = 200000
max_repo_bytes = 1000000000
checkpoint_on_clean_exit = true
reorganization_requires_exclusive_lease = true

[scratch]
max_bytes = 2000000000
max_files = 10000
cleanup_on_exit = true
cleanup_on_crash = true

[publishing]
max_blob_bytes = 100000000
allowed_media_types = ["text/markdown", "text/html", "application/pdf"]
default_visibility = "private"
public_requires_gate = true
```

Worker overrides may make limits stricter. The worker cannot increase quotas,
disable validation, bypass publication policy, or grant itself reorganization
mode.

## Owner interface

The exact command names may change, but the owner needs at least:

```text
roster knowledge status <worker>
roster knowledge log <worker> [--limit 20]
roster knowledge show <worker> <commit>
roster knowledge diff <worker> <commit>
roster knowledge pending <worker>
roster knowledge resolve <worker> <pending-id>
roster knowledge reorganize <worker> <task-file>
roster blobs ls [--worker <worker>]
roster blobs show <blob-id>
```

`roster runs show` should display the knowledge base commit, write mode, produced
commit, integration state, fetch receipt IDs, and published blob IDs.

## Security invariants

- Each worker has a distinct canonical knowledge repository.
- Every run gets an isolated checkout pinned to a recorded base commit.
- Boxes cannot mutate Git metadata, refs, hooks, or the canonical repository.
- Normal concurrent runs cannot modify paths that existed at their base commit.
- Append jobs write only new host-allocated paths under `records/`.
- Only one reorganization writer exists for a worker at a time, and only that
  mode changes existing paths under `organization/`.
- Append and reorganization write sets remain disjoint, so valid commits rebase
  without conflicts.
- Integration is serialized per worker and never resolves conflicts by dropping
  a side.
- Knowledge files are data and cannot register instructions, tools, skills,
  hooks, or runtime policy.
- Scratch content is private to one run and is deleted after terminal handling.
- Publishing copies only the exact bytes reviewed by the policy/gate decision.
- Blobs are immutable; aliases cannot replace historical bytes.
- Journal pointers identify Git commits, fetch receipts, and blobs without
  duplicating their content.
- Knowledge notes never enter worker/channel/user interaction memory.
- Neither notes nor published documents participate in authorization.

## Failure behavior

- Checkout creation failure prevents a knowledge-writing run from starting.
- Notes validation or commit failure leaves canonical `main` unchanged.
- Integration conflict preserves both histories and becomes `needs-merge`.
- Reorganization lease loss prevents commit integration until ownership is
  re-established.
- Publish failure leaves the source file in scratch until the operation is
  terminal or the run's failure-retention deadline expires.
- Journal failure fails closed before a fetch receipt, notes integration, or
  publication is reported as successful.
- Scratch cleanup failure is recorded and retried by supervisor housekeeping.

## Build order

### 1. Repository isolation and host commits

- Create one canonical Git repository per worker.
- Provision isolated per-run checkouts with read-only Git metadata.
- Implement diff validation, trusted commit trailers, and final checkpoint.
- Record base and produced commits in run history.

### 2. Append-mode integration lane

- Create the `records/` and `organization/` write domains.
- Allocate collision-free record IDs on the host.
- Enforce unique added paths and add-only diffs for append runs.
- Serialize rebase/fast-forward integration per worker.
- Treat any conflict between valid commits as an invariant failure and preserve
  it for owner repair.
- Add checkpoint and crash-quarantine behavior.

### 3. Scratch lifecycle and fetch receipts

- Provision isolated quota-bound scratch directories.
- Journal governed fetch receipts with hashes and transient pointers.
- Delete scratch on clean exit and retry cleanup after crashes.

### 4. Governed publication

- Add immutable blob storage and policy evaluation.
- Freeze reviewed bytes across a gate decision.
- Journal blob IDs, hashes, versions, visibility, and provenance.

### 5. Exclusive reorganization mode

- Add the per-worker reorganization lease.
- Drain already queued integrations before taking the reorganization snapshot.
- Allow validated changes under `organization/` while append jobs continue.
- Rebase concurrent append commits and the reorganization commit in either
  integration order without conflicts.

### 6. Naming cleanup and inspection

- Rename interaction-memory storage and CLI away from `notes`.
- Add knowledge and blob owner inspection commands.
- Extend `runs show` with the complete content-pointer lineage.

## Acceptance tests

1. Two concurrent runs for one worker receive different writable checkouts at
   the same recorded base commit.
2. Concurrent append-mode runs adding unique notes integrate without conflict or
   lost files.
3. An append-mode run that modifies an existing file is rejected before commit.
4. The host never allocates the same record path to two runs; a manually
   duplicated ID is rejected during validation.
5. A worker cannot write another worker's repository or select its base commit.
6. The box cannot modify `.git`, hooks, refs, or the canonical repository.
7. A symlink or path escape in a knowledge diff is rejected.
8. A clean run exit checkpoints valid notes before deleting scratch.
9. An abnormal exit quarantines partial note changes instead of integrating
   them.
10. A reorganization run starts after earlier queued integrations drain, while
    append-mode research runs may continue writing their isolated checkouts.
11. Append commits created before, during, and after a reorganization all rebase
    cleanly because their paths do not overlap the reorganization write set.
12. A fetch receipt records URL, time, media type, size, hash, and transient
    scratch pointer without copying the body into the journal.
13. Scratch belonging to one run is not visible to another and is removed after
    exit.
14. A gated publication stores exactly the reviewed bytes and returns an
    immutable blob ID and hash.
15. Publishing another version never overwrites an earlier blob.
16. Journal entries link fetch receipts, Git commits, and blobs into one
    attributable chain.
17. Knowledge repository content never appears in the interaction Memory block.
18. A note named `AGENTS.md`, a Git hook, or a pi configuration file cannot alter
    runtime instructions or tool registration.
19. Gateway grants, gates, budgets, and action behavior are unchanged by notes
    or publication content.

## Open decisions

These choices are intentionally not settled by this spec:

1. **Raw source retention.** Are URL, retrieval time, hash, and selected excerpts
   sufficient after scratch cleanup, or is a fourth private source archive
   required for reproducibility?
2. **Checkpoint cadence.** Is clean-exit checkpointing enough, or should long
   runs checkpoint automatically after a time or byte threshold?
3. **Invariant repair authority.** If an external commit or software defect
   creates a Git conflict, must the owner repair it directly, or may a restricted
   repair run propose a change for owner review?
4. **Remote backup.** Is the canonical Git repository local-only, mirrored to a
   private remote, or backed up through a separate host mechanism?
5. **Published aliases.** Who may update a logical `latest` URL, and does that
   require the same gate as publishing public bytes?
6. **Git-history deletion.** What owner-only process handles a secret or private
   datum that must be purged from repository history rather than merely deleted
   in a new commit?
