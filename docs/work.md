# Work: the task management system

All work a worker does is a **task** in its partition of the task management
system (TMS) — one document per worker (`data/workers/<name>/tasks/
tasks.json`) holding pending tasks and recurring templates, plus an
append-only journal of everything finished. The dispatch loop inside
`roster server start` is a dumb executor over it: each tick it asks the TMS
who is due and runs one governed box per claimable task. Nothing works
inline, and the TMS is the only path into a container. (Chat conversations
take a warm shortcut — see [channels.md](channels.md) — but everything else
is a task.)

## How work arrives

- **You file it**: `roster worker task add yuko "…"`. Owner-standing work
  always runs, budget or no budget.
- **The heartbeat fires.** Every worker has a system recurring template —
  on by default, tuned with `heartbeat = "30m"` in its spec (`"off"`
  disables) — that wakes it to curate its task list and do what's due.
- **A recurring template fires.** Templates live in the partition itself
  (5-field cron, host-local time, optional window) and spawn ordinary
  pending tasks. The worker creates and retires its own via `set_tasks`;
  cursors persist, so a restart neither double-fires nor silently skips.
- **A message arrives.** Chat listeners file relay tasks framed as untrusted
  content — the message directs attention; it never commands (see
  [channels.md](channels.md)). `roster worker task relay` does the same by
  hand.
- **The worker files it.** The `file_task` action adds one task (optionally
  scheduled with `at`); the `set_tasks` action reshapes the whole partition
  — reorder, reschedule, chain with `depends_on`, cancel, recur. Standing
  follows the room: filed at a trusted operator's ask → owner (always
  runs); autonomous or untrusted-context filings → proactive, paced by
  budget.
- **A gate resolves.** If the action's grant sets `wake_on_resolve`,
  resolution files a continuation task, and the next run starts briefed with
  the outcome.

`roster worker run` is the one exception: a governed session right now,
bypassing the TMS, for ad-hoc work and testing. Same box, same gateway,
same rules.

## Task lifecycle

```
pending → claimed → completed | failed | needs-review → journal
```

- Eligibility is derived, never stored: a pending task is claimable once
  its `scheduled_at` (if any) has passed and everything in `depends_on`
  completed. A dependency that failed or was canceled blocks its
  dependents until someone curates the list.
- **`needs-review`** — the box finished but left a gate pending; when the
  last gate resolves, the task completes.
- Over-budget proactive work is simply **late**: it stays claimable and
  runs when the window clears — there is no parked state.
- **`roster worker task requeue`** puts a claimed (dead-box) or
  needs-review task back to `pending`. A completed, failed, or canceled
  task lives in the journal; requeueing it re-files the same task as
  `pending` — the natural retry after fixing what broke it (a missing
  credential, a bad repo path).
- Every state change is host-attested — the worker never marks its own
  work done.

Dispatch polls every couple of seconds and runs up to `--cap` boxes at once
(default 3). On startup it reclaims honestly: `claimed` tasks whose
container actually died go back to pending; live ones are left alone.
Restarting the daemon mid-flight loses nothing. With no model credential in
reach (the vault, a host pi login, or `ANTHROPIC_API_KEY`), dispatch holds —
tasks stay `pending` instead of failing on arrival — and resumes on its own
once a credential appears.

Every run has a wall-clock ceiling — the container is killed when it
expires. Defaults: 30 minutes for filed tasks and ad-hoc runs, 20 for
recurring templates, 15 for relays; override per task with `--ceiling`.

## The worker's own view

Every run mounts the partition read-write at `/opt/roster/tasks.json`
(`$ROSTER_TASKS_FILE`) — a live view the host rewrites on every change.
The worker reads it freely; authoritative writes go through `set_tasks`
with optimistic concurrency (send back the `version` you read; on conflict
re-read and retry). Direct file edits are scratch. The heartbeat template
is `system: true` and host-owned — the one entry the worker cannot touch.

## Code tasks

```bash
roster worker task add yuko --repo ~/projects/site "fix the RSS date bug"
```

The dispatcher provisions a writable git worktree of that repo at `--base`
(default `main`) and mounts it into the box. The worker edits and then calls
`propose_changes` — a gated action whose diff rides on the gate for review.
On approval, the trusted side commits, pushes an `worker/<name>/…` branch, and
opens the pull request. The box never holds the push credential.

## Reorganization tasks

```bash
roster worker task add yuko --reorganize "rebuild the topic index"
```

Takes the worker's exclusive knowledge-reorganization lease: the run may
rebuild `organization/` in the knowledge repo while ordinary append runs
continue alongside. One reorganization at a time per worker; see
[knowledge.md](knowledge.md).

## Runs: the permanent record

Every session — queued, ad-hoc, or chat — is a **run** with a durable
record. Run ids are timestamps with a random suffix
(`2026-07-10-21-51-17-a3b3`); any unique prefix works in commands.

```bash
roster server runs ls --worker yuko
roster server runs show 2026-07-10       # transcript, journal, knowledge
                                          # commits, files, how it ended
roster server runs context <run>         # the exact compiled prompts
roster server runs recall <run>          # why each memory was included
```

A run record answers how the session ended (`done`, `ceiling`, `error`,
`idle`), what knowledge mode it had and what commit it produced, what it
proposed, and what everything cost. The per-worker journal threads the story
across runs: what the box did, what was gated, who decided, what the
executor reported. Nothing here is model-written; it's all computed from
what actually happened.
