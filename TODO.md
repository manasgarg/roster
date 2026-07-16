# To-do

## CLI simplification

- [x] **Make `init` implicit and automatic.** *(shipped 2026-07-16)*
  `cli::init::ensure()` runs quietly on every command: roots created on
  demand, starter org.toml written when absent (never overwrites), one
  stderr line announces a fresh deployment. `roster init` stays as the
  harmless loud form. First-run is: install → `roster worker init yuko` →
  go.

- [x] **Bootstrap LLM credentials on launch.** *(shipped 2026-07-16)* When roster starts and the
  vault holds no LLM-provider credential:
  1. If a host pi login exists (`~/.pi/agent/auth.json`), **ask the user
     to confirm** before importing it (→ vault entries for openai-codex /
     anthropic) — "found a pi login for openai-codex; use it for roster?
     [y/N]". Never import silently.
  2. If there is no pi login to import (or the user declines), walk the
     user through the provider login right there — ask which provider
     (Anthropic or OpenAI) and run the existing `credential add` flow
     (PKCE / device code) inline.
  3. On a non-interactive launch (daemon under systemd), do neither —
     print the hint and skip.

  Decided and documented (getting-started.md): **import-and-own** — after
  import, roster's gateway owns the refresh; pi re-logs-in when it next
  needs to.

- [x] **`roster talk` — the terminal as a first-class channel.** *(shipped)*
  A chat command with the Discord/Slack interaction model, not a bare REPL:
  the terminal becomes a third channel platform, reusing the
  channel-id-keyed machinery — recorded history under `data/channels/<id>/`,
  a purpose, channel + user memory scopes, warm-session turns. Trusted like
  a DM (it's the operator's own shell); replies print directly in the
  terminal. Existing `worker chat` stays as the bare-REPL test harness.

- [x] **`roster talk` starts the server when it isn't running.** *(shipped)*
  Talk probes the gateway port before opening the session; when it's down
  it asks — "the server isn't running — start it now? [y/N]" — then spawns
  `roster server start` detached (own process group, logs to
  `state/server.log`) and waits for the port. Never silent: decline (or a
  non-interactive pipe) prints the `roster server start` hint and exits.

- [x] **Slash-command parity with the CLI.** *(shipped)* Added to the one
  grammar in `channel::slash` (terminal + Discord registration):
  `/gates show`, `/task add|show|requeue`, `/runs ls|show`,
  `/worker show|trust`, `/channel show`, `/memory ls <scope>`.
  Deliberately CLI-only: server lifecycle (`start/status/validate`),
  credentials, connections, `worker init`, `channel set` raw keys,
  `runs context/recall` (dump whole prompts — too big for a chat message),
  and memory pin/unpin/enable/disable/compact (repair tooling).

## Worker autonomy — the task management system

One subsystem (the TMS) owns all scheduling state and logic; the
supervisor is a dumb executor consuming a precomputed due view; the agent
curates its own partition through a read file + one OCC write. Full
behavioral spec and decision log:
[docs/specs/task-management.md](docs/specs/task-management.md). Shipped
2026-07-16, all items below; the `set-tasks` grant is in org.toml and the
roster-box image is rebuilt with the agent-side tools. Deployment fully
migrated.

- [x] **TMS core: the partition store.** *(shipped 2026-07-16)* One document per worker under
  `data/workers/<name>/tasks/` (`version`, `tasks[]`, `recurring[]`) plus
  an append-only `journal.jsonl` for completed/failed tasks and expired
  templates. Mutations: `add(task)` (validation + participant scan on
  tainted filings + due-index update) and
  `set_tasks({base_version, tasks, recurring})` (OCC: apply iff version
  matches, else reject with current doc; validation: schema, acyclic DAG,
  `system: true` entries untouched, no agent-side state jumps, scan on
  prompt diffs from tainted runs). Lifecycle: pending → claimed →
  needs-review/completed/failed; journal on the terminal states.

- [x] **Scheduling engine + due index.** *(shipped 2026-07-16)* Eligibility = `scheduled_at`
  passed (absent: immediate for user-filed, immediate-but-proactive for
  agent-filed) AND all `depends_on` completed. Recurring templates:
  full 5-field cron (host-local), optional `window {from, until}`,
  `spawn_policy: skip-if-previous-open | always`, spawn children carrying
  `recurring_id`, expire to journal at window end. Per-worker
  `(next_due, claimable_set)` precomputed at every mutation — the tick
  never scans.

- [x] **Supervisor integration.** *(shipped 2026-07-16)* The tick calls `due()`; owner-standing
  tasks first, proactive filtered by budget (over budget = simply late —
  the flat queue's `deferred`/revive dance retires); one box per task
  (decided 2026-07-16; batching later); claim stamps `run_id` before box
  start; clean exit → completed, pending gate → needs-review, crash →
  failed; boot reclaim returns dead-container claims to pending and
  rebuilds the index (pure cache). Migration: existing queue files import
  as pending tasks; `[[trigger]]` blocks become recurring templates or a
  config error.

- [x] **Box surface.** *(shipped 2026-07-16)* `/opt/roster/tasks.json` mounted read-write as the
  host-refreshed *view* of the partition (journal excluded; file edits
  are scratch, never authoritative). `set_tasks` and `add` actions
  (`add` replaces `file_task`; same scan). Standing inherits the room
  (decided 2026-07-16): trusted operator's ask → `standing: owner`,
  never budget-paced; untrusted/autonomous → `proactive`. The agent may
  edit user-filed tasks freely — safety by OCC + validation + scan +
  journal, not prohibition (P2/P4).

- [x] **Heartbeat as a system template — and `[[trigger]]` retires.** *(shipped 2026-07-16)*
  The TMS maintains one `system: true` recurring template per worker
  (default on for every worker; interval via `heartbeat = "30m"` in
  worker.toml, admin-owned; `set_tasks` cannot touch it) with the
  curation prompt: review your tasks and your users' asks, reshape the
  list, exit immediately if nothing needs doing. When this ships,
  `[[trigger]]` retires as a public surface (decided 2026-07-15); a
  leftover block becomes a config error pointing at `heartbeat`.

- [x] **Prompt teaching.** *(shipped 2026-07-16)* Rewrite the "Conversations and tasks" section
  of `runtime_policy` (context.rs) around the TMS: your plan lives in
  /opt/roster/tasks.json and only there; reshape it with set_tasks
  (re-read and retry on version conflict); schedule with `scheduled_at`,
  chain with `depends_on`, recur with cron templates; you'll be woken at
  least every N minutes regardless; completion is attested by the host,
  never self-declared.

- [x] **CLI/slash rendering.** *(shipped 2026-07-16)* `task ls/show` (CLI + slash) renders the
  TMS partition: state, scheduled time, dependencies, standing, recurring
  templates; the journal backs "what has been done".

- [x] **Output routing: results return to the origin channel.** *(shipped 2026-07-16)* (Spec:
  "Output routing" in docs/specs/task-management.md, decided 2026-07-16.)
  1. `Tags` gains `provider`; every filing surface sets it (slash /task
     add, file_task, relay) alongside `channel`/`user`.
  2. Dispatch threads the origin to the box as routing metadata — a new
     field on the context request, NOT `RunContext.channel_id` — so the
     surface prompt says "deliver results to <provider> channel <id> with
     <tool>" while the run stays clean (file_task's writable shelf is
     untouchable; the tainting `task.context.discord` path remains only
     for relays).
  3. New `term_send` executor: appends the worker's outbound message to
     the terminal channel's history; `roster talk` prints worker messages
     newer than the operator's last turn on session open ("while you were
     away").
  4. `message_user` demotes to fallback (no origin channel only).
  5. Chunk channel sends and lead DMs at platform message limits
     (Discord 2000) on line boundaries — the failure that actually
     swallowed the 2026-07-16 tech-news report.

Superseded on the way here (see the spec's decision log and non-goals):
the freeform desk as schedule of record, the alarm register and
`wake_at`, `not_before` on flat-queue tasks, per-verb curation tools, and
cron-in-`[[trigger]]` (recurrence returns as agent-editable TMS data —
its proper home). The desk may return someday as a non-load-bearing
notes surface.
