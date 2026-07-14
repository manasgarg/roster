# Knowledge

What a worker learns about the world lives in a private **git repository** —
one per worker, owned by the trusted side. The worker writes notes; the host
validates them and makes the commit. History cannot be rewritten from
inside a box, and `git log` is the audit trail of how the worker's knowledge
grew.

Knowledge is world-space: vendor facts, paper summaries, research records.
It is deliberately separate from [interaction memory](memory.md)
(person-space), and the border between the two is enforced by provenance —
see *The boundary* below.

## The repository

`roster worker init` creates a bare repo at
`data/workers/<name>/knowledge/repo.git` with two top-level areas:

- **`records/`** — immutable source notes and synthesized records, one idea
  per file, with frontmatter (`id`, `created_at`, `source_urls`).
- **`organization/`** — mutable indexes, topic maps, and navigation,
  rebuilt by reorganization runs.

It's ordinary git. Inspect it any time:

```bash
git -C "$(roster worker knowledge yuko)" log --oneline
git clone "$(roster worker knowledge yuko)" /tmp/yuko-knowledge
```

## How runs write it

A run never touches the canonical repo. The host gives each run a plain
checkout — no `.git`, no refs, no hooks — mounted at
`/opt/roster/knowledge`, plus a host-issued **record namespace** its new
filenames must carry. On a clean exit, the host validates the checkout and
integrates it as a commit on `main`; the commit message records the worker,
run, task, base commit, and mode.

Three modes:

- **Append** (normal research runs): add files under `records/` only, each
  ending in the run's namespace. No edits, no deletions, nothing under
  `organization/`, no symlinks or hidden paths, size and secret checks —
  any violation and the integration is refused with the canonical `main`
  untouched.
- **Reorganization** (`worker task add --reorganize`): holds the worker's
  exclusive lease; may rebuild `organization/` and add new records, but
  existing records stay immutable. Append runs continue alongside — their
  write sets are disjoint by construction.
- **Read** — the mount is read-only. This is what tainted runs get (below).

Because concurrent append runs add unique namespaced paths from independent
snapshots, their commits replay onto the latest `main` without conflicts,
in a serialized per-worker integration lane. An abnormal exit quarantines the
checkout instead of integrating it; a validation failure preserves it for
repair and records why in the run record and journal.

`roster server runs show <run>` displays a run's knowledge mode, base
commit, namespace, checkpoint state, and produced commit.

## The boundary: person-space never leaks into world-space

Memory and knowledge have different governance for good reason, and the
leak is asymmetric: knowledge read in a conversation is harmless;
conversation content written into knowledge is a consent bypass into a
store that is deliberately hard to erase. No text filter closes that —
paraphrase defeats every scanner. So the boundary is an information-flow
property, enforced by provisioning:

**Knowledge may only be written by runs that never contained person-data.**

Every run is either **tainted** — it carries interaction content: channel
sessions, relay tasks, continuations with channel context, anything with
memory recall — or **clean**: trigger-fired, admin-filed, self-filed, or
ad-hoc, with a prompt and no participants.

| Run | Knowledge mount | Memory recall |
|---|---|---|
| tainted | read-only | yes |
| clean | writable | none |

The model never chooses its privilege; the run's provenance does. Writing
to the shelf mid-conversation fails at the filesystem. Reading it is fine —
world→person is the safe direction.

**The bridge.** A conversation that surfaces something worth durable
research files a task (`file_task`) instead of writing records. The filed
task is worker-only by construction, so it runs clean with a writable shelf.
That makes the task prompt the entire residual leak surface — one
paragraph per crossing, journaled and scannable — instead of a wide border
of bulk file writes.

**The scan.** The host knows exactly who was in the filing run, so it
checks `file_task` prompts from tainted runs (and new records at
checkpoint) against those participants: their ids, display names, mention
syntax, email addresses. A hit is denied with a legible reason — "names a
conversation participant; that belongs in memory" — and journaled. Stated
honestly: paraphrase still passes the scan. The scan polices the choke
point; the hard guarantee is the mount.

The default policy is `write_from = "clean-room"` in `org.toml
[knowledge]`; `"any-run"` reverts to scan-only behavior for deployments
that accept the tradeoff. Limits (file size, repo size, checkpoint
behavior) are in [configuration.md](configuration.md); per-worker overlays can
tighten them but never loosen.

## Current limitations

- No mid-run checkpoint — knowledge integrates on clean exit only.
- No owner command yet for repairing a quarantined integration candidate,
  and no redaction command for records that made it into history; both are
  git surgery by hand today.
- No automatic remote mirror or backup of the knowledge repos — they're in
  the data root, which is your backup set.
- Source bytes aren't archived: records cite URLs, which is provenance, not
  proof the source still says what it said.
