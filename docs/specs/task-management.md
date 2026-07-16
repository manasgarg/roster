# Spec: task management

The behavioral contract for tasks, in given/when/then form, built around
one dedicated subsystem: the **task management system (TMS)** —
implemented 2026-07-16; every scenario below holds in the tree. Principles
P1–P5 are in [security.md](../security.md).

Three roles, one store:

- **The TMS** owns *all* scheduling state and logic: the task list with
  lifecycle, dependencies (DAG), scheduled times, recurring templates, the
  journal, and a precomputed who-is-due index. Nobody else holds a plan or
  a timer.
- **The supervisor** is a dumb executor: on each tick it asks the TMS
  "who should be woken now, for which tasks," applies its own envelope
  (cap, budgets), runs boxes, and reports claim/complete/fail back.
- **The agent** is the curator of its own partition: it reads the full
  list as a mounted file and writes it back through one optimistically-
  concurrent call, reshaping its plan per its judgment and its users'
  instructions.

Invariants:

1. *Scheduling state lives only in the TMS.* The supervisor holds no plan
   and no timer; the agent holds no private schedule the operator can't
   see.
2. *The TMS decides who is eligible; the supervisor decides who can
   afford to run.* (TMS knows nothing about budgets; the envelope is the
   supervisor's.)
3. *Tasks are partitioned by worker (P1) and tagged with channel/user for
   attribution — tags never partition.*

---

## 1. The data model [today]

One partition document per worker (persisted under
`data/workers/<name>/tasks/`), plus an append-only journal:

```jsonc
{
  "version": 41,                    // OCC counter, bumped on every mutation
  "tasks": [
    {
      "id": "t-8f3a",
      "prompt": "Draft the vendor comparison; sources in knowledge",
      "state": "pending",           // pending | claimed | needs-review | completed | failed
      "created_by": "user",         // user | agent | recurrence | relay | gate
      "standing": "owner",          // owner | proactive (the filer's-standing rule)
      "tags": { "channel": "term-manas-yuko", "user": "manas" },
      "context": null,              // reply routing etc. (e.g. discord channel), as today
      "scheduled_at": "2026-07-18T09:00:00Z",  // absent = see "no time" rule
      "depends_on": ["t-11c2"],     // DAG edges; acyclicity enforced at write
      "ceiling_min": 30,
      "recurring_id": "r-04aa",     // set on children spawned from a template
      "run_id": null                // stamped at claim
    }
  ],
  "recurring": [
    {
      "id": "r-04aa",
      "prompt": "Morning digest: scan the feeds, post a summary",
      "schedule": "0 9 * * 1-5",    // full 5-field cron, host-local time
      "window": { "from": "2026-07-16", "until": "2026-09-01" },  // optional
      "spawn_policy": "skip-if-previous-open",  // or "always"
      "standing": "proactive",
      "ceiling_min": 20,
      "tags": { "channel": "discord-123" },
      "system": false               // true = host-owned (heartbeat); set() cannot touch it
    }
  ]
}
```

**Recurring templates have no lifecycle.** A template spawns a child
`pending` task at each firing (child carries `recurring_id`); only
children move through states. `skip-if-previous-open` suppresses a spawn
while the previous child is still pending/claimed — no pileups. When
`window.until` passes, the template expires to the journal.

**The journal** (`journal.jsonl`, append-only, P4): every task that
reaches `completed` or `failed`, and every expired template, is appended
with its full record and pruned from the live document — the live file
stays small because the agent reads it every run.

## 2. Lifecycle [today]

```
                     (spawned by recurrence)
user/agent/relay ──→ pending ──→ claimed ──→ completed ──→ journal
                        ▲            │
                        │            ├──→ needs-review ──→ completed
                        │            │    (left a gate; resolves)
                        └────────────┴──→ failed ──→ journal
                                          (crash / ceiling)
```

**Eligibility (derived, never stored)**
- Given a `pending` task
- Then it is *claimable* iff its `scheduled_at` (if any) has passed AND
  every task in `depends_on` is `completed`
- And "blocked" is a view, not a state: an unmet dependency simply keeps a
  task out of the claimable set.

**The "no time" rule**
- Given a task with no `scheduled_at`
- Then if `created_by` is a user/admin → it is eligible immediately
- And if `created_by` is the agent (or a recurrence with
  `standing: proactive`) → it is eligible immediately but paced by the
  budget envelope — "anytime" means "whenever there's room," enforced by
  standing, not by time.

**Dependencies and failure**
- Given task B with `depends_on: [A]`
- When A completes → B becomes claimable (index updated at that moment)
- When A fails → B stays pending and unclaimable; the agent (or user)
  resolves it at the next opportunity by editing the list — retry is
  judgment, not mechanism
- And a write that would create a cycle is rejected whole.

## 3. The interface [today]

Four calls; nothing else exists.

**`add(task)`** — the single filing path for everyone
- Given any filer — admin CLI/slash, a relay, a gate continuation, a
  recurrence firing, or the agent in-box (this replaces `file_task`)
- When a task is added
- Then it is validated (schema, DAG), scanned (a prompt from a tainted
  run passes the participant scan or is refused with a legible, journaled
  reason), stamped with `created_by`/`standing`/`tags`, and the due index
  is recomputed
- And the surfaces are `task add`/`/task add`, `file_task` (one task,
  optional `at`), relays, and `wake_on_resolve` continuations.

**`get(worker)` — the mounted file**
- Given any run of worker `yuko`
- Then `/opt/roster/tasks.json` is mounted read-write, rendering that
  worker's pending + recurring entries (journal excluded)
- And the host rewrites it whenever the partition changes, so even a warm
  session reads fresh state mid-conversation
- And the mount is a *view*: edits to the file are scratch convenience,
  never authoritative (the host's next refresh overwrites them) — the
  authoritative write is `set_tasks`.

**`set_tasks({base_version, tasks, recurring})` — curation** *(decided
2026-07-16: read-file + action, not file-write-as-set)*
- Given an agent that has read the document at `version: N`
- When it calls `set_tasks` with `base_version: N` and the reshaped lists
- Then the TMS validates: schema; DAG acyclic; `system: true` entries
  untouched; state transitions legal (an agent cannot mark tasks
  completed — completion is supervisor-attested); and prompt diffs from a
  tainted run pass the participant scan
- And the write applies iff `base_version` equals the current version;
  otherwise it is rejected with the current document, and the agent
  re-reads and retries (OCC)
- And the agent may edit **any** entry in its partition — including
  user-filed tasks — freely *(decided 2026-07-16: "move my report to
  Friday" said in a conversation is the main use case; safety comes from
  OCC + validation + the scan + the journaled, versioned history, not
  from prohibition — P2/P4)*.

**`due()` — the supervisor's whole view of the world**
- Given the precomputed index (updated at every add/set/spawn/claim/
  complete — never scanned on the tick)
- When the supervisor polls
- Then the TMS answers `[{worker, claimable: [task, …]}]` — eligibility
  only; standing is on each task, budgets are none of the TMS's business.

## 4. The supervisor plane

**The tick** [today]
- Given the daemon is running
- When each poll tick occurs (every 2s, and immediately when a box exits)
- Then dispatch: pauses entirely if config is invalid (fail closed);
  asks `due()`; and for each claimable task, in order — owner
  standing first, then proactive — starts a box when a slot is free and
  (for proactive) the worker is not over any budget limit
- And an over-budget proactive task is simply late: it stays claimable
  and runs when the envelope allows (no parked state needed — [today]'s
  `deferred`/revive dance retires with the flat queue).

**One task, one box** *(decided 2026-07-16; batching is a possible later
optimization)*
- Given a claimable task the supervisor can afford
- When it dispatches
- Then the TMS marks it `claimed` and stamps the `run_id` before the box
  starts (a crash leaves a mappable orphan), and exactly one governed
  one-shot container runs exactly this task
- And on clean exit → `completed` (journaled); on a pending gate →
  `needs-review`, completing when the last gate resolves (and
  `wake_on_resolve` may `add` a continuation); on crash/ceiling →
  `failed` (journaled).

**Crash recovery** [today; same shape under TMS]
- Given the daemon restarts with `claimed` tasks
- When reclaim runs at boot
- Then tasks whose container is actually dead return to `pending`; live
  ones are left alone
- And the due index is rebuilt from the partition documents (it is a pure
  cache).

**Execution shape** [today; desk-era items superseded]
- Given any run
- Then it executes in a fresh one-shot container: compiled context,
  knowledge shelf per provenance (writable `append` for clean runs,
  read-only for tainted), the tasks view mounted, wall-clock ceiling
  enforced by killing the container; every outcome attributable via run
  log and journal.

## 5. The agent plane [today]

**Curation is the autonomy**
- Given the worker is invoked — for a task, a heartbeat, or a
  conversation turn
- Then its plan is fully legible to it in `/opt/roster/tasks.json`, and
  fully malleable through `set_tasks`: reorder, reschedule, cancel, chain
  with `depends_on`, create or retire recurring templates (its own cron,
  kept as data, not config)
- And "wake me at T to do X" is nothing special: an agent-added task with
  `scheduled_at: T` — the TMS subsumes the alarm.

**The heartbeat floor**
- Given any worker — the floor is on by default, interval tunable via
  `heartbeat = "30m"` in its spec (admin-owned)
- Then the TMS maintains a `system: true` recurring template per worker
  with the curation prompt: review your tasks and your users' asks,
  reshape the list, exit immediately if nothing needs doing
- And `set_tasks` cannot modify or remove it — the floor is the
  supervisor's guarantee, living in the same store as everything else
- And therefore the scheme is self-healing: the due index is a cache, the
  document is the truth, and any confusion is at most one heartbeat from
  repair.

**Conversations** [today]
- Given a user in a conversation asks for future or clean-room work
- When the agent files or reshapes tasks accordingly
- Then the standing inherits the room: a trusted operator's ask →
  `standing: owner` (never budget-paced — the human asked); an untrusted
  context or autonomous initiative → `standing: proactive`, budget-paced,
  so induced filings cannot bypass the envelope.

## 6. Output routing [today]

**Results go back to the room that asked — never to a person by default**
(decided 2026-07-16).

**A task from a channel reports to that channel**
- Given a task filed from any channel — Discord, Slack, or the terminal —
  its `tags` carry the origin (`provider` + `channel`)
- When the task runs
- Then the origin is threaded to the box as **routing metadata, not
  provenance**: the run's surface prompt names the reply channel and the
  right send tool, while the run context stays clean — a scanned
  `file_task` prompt must keep its writable knowledge shelf, so routing
  must never taint (the existing `task.context.discord` threading, which
  does taint, remains only for relay-style tasks that genuinely carry
  interaction content)
- And the delivery is an ordinary governed act: channel sends inherit the
  channel's trust designation (untrusted → gate), exactly as live replies
  do.

**The terminal is a real reply target**
- Given a task whose origin channel is a terminal channel (`term-…`)
- When the worker delivers results
- Then a `term_send` executor appends the message to that channel's
  recorded history as the worker's own outbound message
- And the next `roster talk` on that channel surfaces what arrived while
  the operator was away ("while you were away", worker messages newer
  than the operator's last turn) — the terminal channel becomes durable
  in both directions, not inbound-only.

**No origin, then the lead ladder**
- Given a task with no origin channel (heartbeat curation, autonomous
  initiative, gate continuations of such work)
- Then `message_user` remains the escalation path: lead DM, then the
  inbox — the ladder is the fallback, no longer the default route.

**Long messages are the sender's problem, not the plumbing's**
- Given a report longer than a platform's message limit (Discord: 2000)
- When any channel send or lead DM posts it
- Then the executor chunks at the limit on line boundaries instead of
  failing into the silent inbox (this was the actual failure on
  2026-07-16: a 50035 BASE_TYPE_MAX_LENGTH error dropped a finished
  report into `audit/messages.jsonl` unread).

## 7. Visibility

- Given any moment
- Then `task ls/show` (CLI and slash) shows every task and state,
  `runs ls/show` every execution, `server status` the queue by state
- And the same commands render the TMS partition (including
  scheduled times, dependencies, and recurring templates), the journal
  answers "what has been done," and the operator can read the raw
  partition document — the agent's whole plan — straight off disk.

---

## Decision log

- **2026-07-16 — the TMS replaces the two-plane model.** The desk-as-
  schedule, the alarm register, and `wake_at` are superseded: scheduling
  concerns kept leaking across the queue/desk split, so all scheduling
  state now lives in one purpose-built system, the supervisor consumes a
  precomputed view, and the agent curates its partition directly.
- **2026-07-16 — one box per task.** Batch invocation (one box claiming a
  due pile) deferred as an optimization; per-task boxes keep completion
  attestation and crash isolation trivial.
- **2026-07-16 — read-file + `set_tasks` action.** The mounted file is the
  read view; the authoritative write is an explicit action (atomic,
  validated, journaled, legible rejections). File writes are scratch.
- **2026-07-16 — the agent edits user-filed tasks freely.** Safety by
  OCC + validation + participant scan + versioned journal, not by
  prohibition (P2/P4).
- **2026-07-16 — full 5-field cron for recurrence** (host-local time).
  This un-reverts the 2026-07-15 cron decision *in a new home*: the
  objection was cron as admin config grammar in `[[trigger]]`; recurrence
  as agent-editable data inside the TMS is where cadence was always
  supposed to live.
- **2026-07-16 — `task.context` stays** for reply routing (e.g. the
  Discord channel a queued task answers to); `tags` carry attribution.
- **2026-07-16 — output routes to the origin channel, not the user.**
  Tags graduate from attribution to routing: a task's results are
  delivered to the channel that filed it (terminal included, via
  `term_send` + missed-message display in talk), as routing metadata that
  never taints the run. `message_user` demotes to the fallback for work
  with no origin room. Channel sends chunk at platform limits.
- **2026-07-15 — `[[trigger]]` retires** when the TMS ships: admin
  ceremonies become recurring templates; the heartbeat is the system
  template; a leftover `[[trigger]]` block becomes a config error.

## Non-goals, recorded so they stay decided

- **A freeform desk as the schedule of record** — superseded by the TMS
  partition document; a freeform notes surface may return someday as a
  separate, non-load-bearing concern.
- **`not_before` on flat-queue tasks / `wake_at` / the alarm register** —
  superseded; `scheduled_at` inside the TMS is the one time field.
- **Host-side per-verb curation tools** (`reschedule_task` etc.) — the
  curation surface is one read view + one OCC write, not an RPC verb per
  edit.
- **Per-channel task partitions** — tasks partition by worker (P1); tags
  attribute.
- **Automatic retry of failed tasks** — retry is judgment; the agent or
  user re-adds.
