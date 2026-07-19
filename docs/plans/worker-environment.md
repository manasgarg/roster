# Plan: everything is a connection — the worker environment

Status: implemented (2026-07-19) — supersedes the 2026-07-18
channel-partitioned draft in this file. Shipped docs: store.md, repos.md,
connections.md, box.md, layout.md, channels.md, memory.md, work.md.
Implementation deviations from this draft: the legacy knowledge repo
works as an *implicit* gated connection named "knowledge" (no file
written by migration — behavior-preserving with zero churn); `[channels]`
bindings stay in worker.toml as the listener binding, with scoping via
`[restrict]` on the connection; the code-task worktree flow remains as a
legacy path pending the repo-connection route proving out; host-side
interaction-memory recall still runs — memory-in-store is seeded and
taught, host machinery retires separately.
Scope: make the connection the *only* primitive through which a worker
reaches anything — services, Discord, host directories, host git repos,
its own footprint. Mounts become materialized connection grants. The
worker's environment is identical across every channel it serves; the
only per-channel difference is the conversation history it sees.

## The model

A connection is roster's relationship with a resource — already true for
external services (identity in the vault + uses), now extended to host
resources. Everything a worker can touch arrives the same way: the
connection is **added to the roster first**, then **made available to a
worker** as a grant, possibly scoped. Nothing is ambient.

Connections today have three uses (capability, channel, model —
docs/connections.md). This design adds a fourth:

| Use | What granting gives the worker | Enforcement point |
|---|---|---|
| capability | box may call the service: egress + credential injection | gateway |
| channel | worker speaks through the service (listeners, SMTP) | host-side listener |
| model | gateway injects the credential into model-API calls | gateway |
| **mount** | the resource appears in the worker's filesystem | bind mount at run start |

Existing properties carry over unchanged: several connections may exist
for one service under different names and credentials (`--name` — two
Discord bots are two connections), and the secret never enters a box.

## Connection kinds

| Kind | Backing | Uses | Write model |
|---|---|---|---|
| service | vault secret + provider registry | capability / channel / model | gateway policy |
| **host dir** (new) | a host path | mount | `ro` or `rw` (direct writes) |
| **host repo** (new) | a host git repository | mount | `ro`, or **gated**: branch-per-run + push gate |
| **self** (built-in) | the worker's own footprint | mount | always `ro`; edits via CAS actions |

Every worker is auto-provisioned one **host-dir connection named
`store`** (`data/workers/<name>/store/`, rw) — its default durable
space. It is a real connection: it shows in `connection ls`, and it can
be revoked or re-scoped like any other. Beyond that, durable space is
whatever the operator grants.

## Grant scoping

A grant may narrow a connection, and one scope governs every use the
connection has:

- **Discord**: limit a worker to a server (guild) or to specific
  channels. The listener refuses to attach outside the scope, and the
  gateway restricts API calls to it — same scope, both enforcement
  points, so "can speak there" and "can act there" never drift apart.
- **GitHub**: limit to an org or repo set (gateway policy, as today).
- **Host dir**: mode (`ro`/`rw`), optionally a subpath. Granting `rw`
  on a dir roster doesn't back up (anything but `store/`) warns at
  grant time: no gate, no snapshots — a bad run's writes there are
  unrecoverable by roster.
- **Host repo**: `ro` or gated-write; optionally the base branch.

There is no universal scope language. A scope is a set of
**provider-declared dimensions** (registry-defined: `server`/`channel`
for Discord, `org`/`repo` for GitHub) that compile down to the two
enforcement points — listener attachment rules and gateway request
predicates (host, method, path pattern; Discord scopes compile to the
guild/channel ids in API paths). For a generic HTTP connection the
scope *is* the gateway predicate vocabulary directly: hosts, methods,
path prefixes. New sugar means a registry entry, not a new mechanism.

## Channels are surfaces, not boundaries

A worker attaches to any number of channels — several Discord channels,
Slack, the terminal (`term` is a channel like any other). Across all of
them the worker is **one environment**: same connections, same mounts,
same store, same identity. The only thing that varies per run is the
conversation history mounted for the channel being served, and the
reply route.

This supersedes the channel-partitioned stores of the previous draft.
The consequence is accepted plainly: with the default store being an
ungated rw directory, conversation content can persist there and inform
the worker's behavior in other channels. Cross-channel discretion is
the worker's conduct (identity.md), not a storage boundary. Where
stronger control is wanted, a gated host-repo connection provides it —
its push gate retains the current validation (secret scan, conversation
participants, bulk-delete gate).

Only the **active channel's** history is mounted; other channels the
worker serves are visible as metadata in `self/`, but their histories
never mount. A DM's content reaches another channel only if the worker
chooses to write it somewhere durable — never by mount.

## Memory

Interaction memory moves into the store: `memory.jsonl` stops being a
host-owned file and becomes the worker's own, kept under `store/`
however the worker organizes it. The host stops composing memory into
run context; instead the briefing teaches the practice — where memory
lives, how to consult it at run start, how to record what deserves
keeping, and the discretion expected of person-facts learned in one
channel when serving another. Migration seeds the store with the
existing `memory.jsonl` as its starting point.

## The worker's filesystem

```
$HOME                                     rw · per-run, ephemeral
├── workspace/                              scratch, default cwd, clones
├── session/                                transcript / session state
│
├── store/                                rw · durable
│   └── (worker-managed — anything)         the auto-provisioned host-dir
│                                           connection; backed up (§Backups)
├── mnt/                                  granted mount connections
│   ├── <host-dir-conn>/                    ro or rw, per grant
│   └── <host-repo-conn>/                   clone on branch run/<id> (gated)
│                                           or ro checkout
├── self/                                 ro · live (built-in connection)
│   ├── worker.toml  identity.md            config, identity
│   ├── schedule.json                       tasks
│   ├── journal/  runs/                     journal, past-run records
│   └── channels/                           attached-channel metadata
│
└── channel/                              ro · live — the active channel:
                                            history.jsonl, purpose,
                                            settings, files/

/tmp                                      rw · 2 GiB tmpfs
/opt/roster/ca.crt, ca-bundle.crt         ro · gateway CA
/opt/roster/engine                        ro · dev mounts only
```

The shape is identical for every run; what varies is which channel dir
binds at `channel/` and which grants populate `mnt/`.

Mechanics, unchanged from the prior draft: writable mounts keep fixed
container paths (F4); home is ephemeral **by design** so no run can
plant dotfiles for the next; `/tmp` stays a capped tmpfs; the vault,
`org.toml`, audit logs, backups, and other workers' footprints are
never mounted.

`workspace/` and `session/` are **plain subdirectories, not mounts** —
one bind, directories inside it. `session/` is a contract path: the
agent harness writes its transcript and session state there, and the
host collects it after the run as the run record — it exists for the
host, not the worker. `workspace/` is only a convention: a default cwd
so scratch and clones don't mix with the namespace roots (`store/`,
`mnt/`, `self/`, `channel/`) and dotfiles, and so "what did this run
leave behind" is one directory to inspect. Nothing breaks if a worker
works elsewhere in `$HOME`.

The host treats every rw-mounted dir as **inert bytes**: rsync, list,
back up — never run git in it, never parse, never execute. A box-written
`.git/config` or hook is an execution vector. The one place the host
touches box-authored git state is the gated push, which is built for it
(bundle into quarantine, fsck, validate — the host never runs git
against the box's clone itself).

## Host repos: the gated push, generalized

The current knowledge design is retained wholesale and becomes the
write model of any gated host-repo connection:

- The run gets a real clone at `mnt/<name>/`, checked out on
  `run/<run-id>`, with the canonical repo read-only as `origin` — a ref
  write from the box is a filesystem error.
- The `knowledge_push` action generalizes to `repo_push <connection>`:
  bundle `origin/main..HEAD`, host receives into quarantine, fscks,
  validates (regular files, size caps, secret scan, conversation
  participants, deletion count vs the bulk-delete gate), then advances
  `main` fast-forward only, serialized per repo by flock. Divergence is
  resolved in the box — fetch, rebase, push again. `git log main`
  stays the audit trail; unpushed work at run end is parked on a
  quarantine ref with a briefing note, as today.

Code tasks get both routes, per repo:

- **Host repo** with a gated-write grant: work lands as validated
  commits on `main` (or as a branch the operator merges) — no push
  authority to any remote needed.
- **Remote repo** via a service connection with a push grant: clone
  into `workspace/`, edit, push a branch through the gateway; keep
  `main` protected so review stays the merge gate.

## Coordination between instances

Unchanged from the prior draft: `flock(2)` across bind mounts (inode-
scoped, kernel-released on crash); a `roster-lock <name> -- <cmd>`
helper in the box image with named locks under `store/.locks/`; bare
repos + ephemeral clones for anything git-shaped (atomic ref updates
make git its own lock); advisory locking accepted as advisory — the
recovery for a skipped lock is a backup restore. Gated repos need none
of this: the host's per-repo integration lane serializes landings.

## Editing the self view

`self/` is read-only; changes go through the server:

- One generic `file_update` action: path, expected **content hash**, new
  content. Server takes the worker's lock, verifies the hash, applies
  per-file validation, writes atomically. Stale hash → clean conflict →
  re-read and retry. (The schedule's CAS, generalized; a hand edit on
  the host invalidates any held token automatically.)
- Editable to start: `worker.toml`, the schedule. `identity.md` requires
  a human gate. Everything else is read-only.

## Backups

- rsync snapshots of `store/` with `--link-dest`: N snapshots ≈ one full
  copy + deltas. After every writable run plus a daily sweep; last 14
  kept (configurable); stored under the data root.
- The snapshot holds the store lock during the copy; the backup pass
  also journals an itemized change summary (`--itemize-changes`) into
  the run log — "what did run X change?" stays answerable.
- Granted host dirs are **not** backed up by roster — they're the
  operator's directories with their own story. Gated repos need no
  snapshots: history *is* the backup.
- Restore is a real command: `roster worker restore <name>
  [--from <snapshot>]`.
- This protects against bad runs, not disk loss — disk loss remains the
  operator's backup of `data/`; layout.md should say so.

## What this changes

| Today | Fate |
|---|---|
| Knowledge as a privileged concept (`knowledge/repo.git`, `ROSTER_KNOWLEDGE_DIR`) | becomes an ordinary gated host-repo connection, opt-in |
| `knowledge_push` | generalized to `repo_push <connection>` |
| Taint/clean-room tracking, `write_from` policy | deleted — no cross-channel storage boundary exists to protect; gated repos keep content validation |
| `/opt/roster/knowledge{,.git}`, `/worktree` + parent-repo mount, `/opt/roster/tasks.json` | replaced by `mnt/<connection>/` and `self/` |
| `/workspace`, `/session`, `/pihome` as three mounts | one `$HOME` |
| Channel bound to worker via `[channels]` only | channel becomes a scoped grant of a (possibly named, multi-bot) Discord/Slack connection |
| Bulk-delete gate on the store | deleted for the plain store (restore-from-snapshot instead); retained inside gated repos |
| `memory.jsonl` host-owned, host-composed into context | worker-owned under `store/`; the briefing teaches the memory practice |

## Migration

Behavior-preserving defaults, no data movement, one command:

- Each existing `knowledge/repo.git` becomes a gated host-repo
  connection named `knowledge`, granted to its worker — same data, same
  audit trail, same push semantics under the new `repo_push` name. The
  operator can later demote it to plain files in `store/` by hand if a
  worker doesn't need the gate.
- Each `[channels]` binding in worker.toml rewrites as a grant of the
  corresponding connection with an unrestricted scope — exactly the
  surface the bot already served; narrowing comes after, at will.
- `store/` is provisioned seeded with the worker's `memory.jsonl`; the
  next briefing tells the worker its memory now lives there and is its
  own to organize.
- Quarantine refs are parked as bundles alongside the knowledge repo;
  nothing is dropped silently.

## Build order

1. Connection kinds: host-dir and host-repo in the registry + grant
   scoping (Discord server/channel first).
2. Mount materialization: grants → `mnt/`, `store/` auto-provision, the
   one-`$HOME` layout, `self/` + `channel/` views.
3. Backups + `roster worker restore` (land with or before the rw store).
4. `file_update` CAS + identity gate.
5. `repo_push` generalization; knowledge repo becomes a connection;
   migration of `[channels]` bindings to scoped grants.
6. `roster-lock` helper + briefing (store tour, locking, bare-repo
   advice).
7. Docs rewrite: connections.md (mount use), box.md, knowledge.md →
   repos.md, channels.md (scoped grants), layout.md.
