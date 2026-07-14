# Imp knowledge repository (implemented specification)

**Status: implemented.** This document describes the system that currently
ships. It does not specify a scratch store, governed download archive, or
publication system.

## Outcome

Each imp has one private Git repository for durable knowledge about the
world. Each run receives an isolated plain checkout of that repository. The
imp may add research notes concurrently with other runs, while the trusted
host validates and integrates changes.

Temporary files are ordinary container files. Pi uses `/tmp` for downloads,
extracted documents, intermediate calculations, and other disposable work.
`/tmp` is not a Impyard storage product and is not a durable content address.

Interaction memory remains separate. It stores continuity about imps,
channels, and users; research sources and world knowledge belong in the Git
repository.

There is currently no publication blob store.

## Storage boundaries

| Surface | Purpose | Lifetime |
| --- | --- | --- |
| `/opt/impyard/knowledge` | Structured world knowledge | Integrated into the imp's Git repository on a clean exit |
| `/tmp` | Downloads and disposable working files | Exists only for the life of the container |
| `memory/<imp>.jsonl` | Imp, channel, and user interaction continuity | Durable interaction memory |
| Journal and run records | Activity, validation, and Git commit pointers | Durable operational history |

The systems do not copy content into one another automatically. In particular:

- temporary files do not become knowledge unless the imp writes a valid
  knowledge record;
- knowledge files do not enter interaction memory;
- interaction memories do not become research notes;
- journal entries point to Git commits but do not duplicate note bodies.

## Canonical repository

Impyard creates one bare Git repository per imp when the imp is created:

```text
knowledge/<imp>/repo.git
```

The initial branch is `main`. The initial tree contains:

```text
records/
organization/
README.md
```

`records/` contains immutable source notes and synthesized records.
`organization/` contains mutable indexes, topic maps, and navigation maintained
by an exclusive reorganization job.

The repository is not mounted into an imp container. A run receives a plain
checkout without `.git`, hooks, refs, credentials, or access to the canonical
repository.

## Per-run checkout

For run `<run-id>`, the trusted host creates:

```text
runs/<run-id>/knowledge/
```

and mounts it at:

```text
/opt/impyard/knowledge
```

The host records:

- imp;
- run and task identifiers;
- base commit;
- write mode;
- a collision-resistant record namespace;
- checkpoint state and produced commit.

The container receives these values through trusted environment variables:

```text
IMPYARD_KNOWLEDGE_DIR=/opt/impyard/knowledge
IMPYARD_KNOWLEDGE_BASE=<commit>
IMPYARD_KNOWLEDGE_MODE=append|reorganization
IMPYARD_RECORD_NAMESPACE=<namespace>
```

## Append mode

Normal research runs use append mode. A run may add files only below
`records/`. Every filename must end with its host-issued namespace and a local
counter:

```text
records/<topic>/<slug>--<namespace>_<number>.md
```

For example:

```text
records/vendors/acme-sso--n_7f3c91a2d0e4_1.md
```

The host rejects a checkpoint if the run:

- modifies or deletes an existing record;
- writes under `organization/`;
- uses a filename outside its namespace;
- adds a duplicate note ID;
- adds a symlink, special file, hidden path, Git metadata, or runtime
  instruction file;
- exceeds the configured file or repository limits.

Each new note is expected to carry stable frontmatter:

```yaml
---
id: n_7f3c91a2d0e4_1
created_at: 2026-07-11T12:00:00Z
source_urls:
  - https://example.com/report
---
```

The URL is useful provenance, but it is not proof that the exact source bytes
remain available.

## Concurrent integration

Runs start from independent snapshots and may finish in any order. The trusted
host validates a run against its recorded base, then enters a serialized
per-imp integration lane.

Valid append runs add unique paths, so their commits can be replayed onto the
latest `main` without textual merge conflicts. A path collision or invariant
failure leaves canonical `main` unchanged and preserves the candidate for owner
repair.

The trusted commit message records imp, run, task, channel, base commit, and
write mode. Git therefore records the history of how knowledge was added and
organized.

## Reorganization mode

A queue task created with `--reorganize` obtains one exclusive reorganization
lease for the imp. It may:

- rebuild files below `organization/`;
- add new immutable records using its own namespace;
- leave all existing records unchanged.

Append runs may continue while reorganization is active because their write
sets are disjoint. Only one reorganization job may run for an imp at a time.
The integration lane still serializes final Git updates.

## Checkpoint and failure behavior

On a clean run exit, the host validates the checkout and, when valid, creates
and integrates a Git commit. If there are no changes, it records an empty
checkpoint outcome.

On an abnormal exit, the checkout is quarantined rather than integrated. A
validation failure or Git conflict leaves canonical `main` unchanged and
records the reason in the run manifest and journal.

Only the clean-exit checkpoint exists today. Mid-run checkpoints and owner
repair commands are not implemented.

## Container temporary files

Every imp container receives a private tmpfs mounted at `/tmp` with:

```text
size=2147483648
mode=1777
nosuid
nodev
```

Impyard sets `TMPDIR=/tmp`. Docker removes the tmpfs when the container exits,
including after a crash or timeout. There is no host-side scratch directory,
scratch cleanup job, scratch run record, or scratch policy.

Pi may use normal tools such as `curl` to download into `/tmp`. Network traffic
still passes through Impyard's governed gateway and is subject to its host,
method, credential, and budget rules. The current gateway records the request
and policy decision, but Impyard does not retain the response body or produce a
trusted receipt containing its hash.

A temporary path must never be written into a note as though it were a durable
content pointer. Notes should cite the original source URL and record relevant
observations. Exact source preservation would require a separate source archive,
which is not currently implemented.

## Interaction memory remains separate

The knowledge checkout is never read by the interaction-memory selector. A
vendor fact, paper summary, or downloaded document cannot appear in the Memory
block merely because the imp created it.

Interaction memory uses `memory/<imp>.jsonl` and `impyard memory`. The legacy
`notes/` storage name and `impyard notes` command remain read-compatible migration
aliases only.

## Configuration

The owner-controlled knowledge settings are:

```toml
[knowledge]
enabled = true
normal_mode = "append"
max_file_chars = 200000
max_repo_bytes = 1000000000
checkpoint_on_clean_exit = true
reorganization_requires_exclusive_lease = true
```

Imp overrides may disable features or reduce limits. They cannot enlarge
org limits, enable a disabled feature, bypass validation, or remove the
reorganization lease.

The container temp size is currently a runtime constant rather than an owner
or imp policy setting.

## Owner interface

```text
impyard knowledge <imp>
```

This prints the canonical bare repository path. The owner can then use normal
Git commands:

```text
git -C "$(impyard knowledge yuko)" log --oneline
git -C "$(impyard knowledge yuko)" show HEAD
git clone "$(impyard knowledge yuko)" /tmp/yuko-knowledge
```

`impyard runs show <run-id>` displays the run's knowledge mode, base commit,
record namespace, checkpoint state, produced commit, and validation error.

## Security invariants

- Imp and run identity come from the trusted host, not tool arguments.
- An imp cannot select its base commit or record namespace.
- The box cannot access canonical Git metadata, refs, hooks, or credentials.
- Append and reorganization write sets remain disjoint.
- Knowledge content never grants capabilities or changes runtime instructions.
- Knowledge and interaction memory remain separate systems.
- Container temporary files disappear with the container and are never treated
  as durable references.

## Current limitations

- No mid-run knowledge checkpoint.
- No owner command for repairing a preserved integration candidate.
- No private remote mirror or automatic backup.
- No persistent archive of downloaded source bytes.
- No publication or blob-store subsystem.

## Verification

Run the automated checks and compile policy:

```text
cargo test
cargo run -- deploy
```

Verify the container primitive directly:

```text
docker run --rm --entrypoint /bin/sh \
  --tmpfs /tmp:rw,nosuid,nodev,size=2147483648,mode=1777 \
  impyard-box -lc 'echo ok >/tmp/probe; stat -f -c %T /tmp; df -h /tmp; cat /tmp/probe'
```

The output should identify `tmpfs`, show a 2 GiB filesystem, and print `ok`.
Because the container uses `--rm`, `/tmp/probe` disappears with the container.

For an end-to-end imp check, start `impyard serve`, then ask an imp to print
`$TMPDIR`, inspect `/tmp`, download a small HTTPS page there, and confirm that
`/opt/impyard/scratch` does not exist. After the run, `impyard runs show <run-id>`
should contain knowledge checkpoint information but no scratch, fetch-receipt,
or blob fields.
