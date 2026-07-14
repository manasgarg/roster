# Work: tasks, triggers, and runs

All work an imp does is a **task** on its durable, per-imp queue — filed by
you, by a schedule, by a chat message, or by the imp itself. The dispatch
loop inside `impyard server start` drains the queues into boxes. Nothing
works inline: a trigger firing or a message arriving *files a task*, and the
queue is the only path into a container. (Chat conversations take a warm
shortcut — see [channels.md](channels.md) — but everything else is a task.)

## How work arrives

- **You file it**: `impyard imp task add yuko "…"`. Admin-filed work always
  runs, budget or no budget.
- **A schedule fires.** Triggers live in the imp's spec and run on an
  interval:

  ```toml
  # imps/yuko/imp.toml
  [[trigger]]
  schedule    = "every 1h"        # units: s, m, h, d
  prompt      = "scan the feeds; file anything worth a deeper look"
  ceiling_min = 20
  ```

  Trigger-filed tasks are **proactive**: budget-gated at dispatch. Cursors
  persist across restarts, so a schedule neither double-fires nor silently
  skips because the daemon rebooted.
- **A message arrives.** Chat listeners file relay tasks framed as untrusted
  content — the message directs attention; it never commands (see
  [channels.md](channels.md)). `impyard imp task relay` does the same by
  hand.
- **The imp files it.** The `file_task` action queues durable follow-up work
  on the imp's own queue — the bridge that lets a conversation schedule
  clean-room research (see [knowledge.md](knowledge.md)).
- **A gate resolves.** If the action's grant sets `wake_on_resolve`,
  resolution files a continuation task, and the next run starts briefed with
  the outcome.

`impyard imp run` is the one exception: a governed session right now,
bypassing the queue, for ad-hoc work and testing. Same box, same gateway,
same rules.

## Task lifecycle

```
waiting → running → done | failed | needs-review
```

- **`needs-review`** — the box finished but left a gate pending; when the
  last gate resolves, the task completes.
- **`deferred`** — a proactive task that hit the budget gate; it waits for
  the window to reset without clogging the queue.
- **`impyard imp task requeue`** puts a stuck or finished task back to
  `waiting`.

Dispatch polls every couple of seconds and runs up to `--cap` boxes at once
(default 3). On startup it reclaims honestly: `running` tasks whose
container actually died are requeued; live ones are left alone. Restarting
the daemon mid-flight loses nothing.

Every run has a wall-clock ceiling — the container is killed when it
expires. Defaults: 30 minutes for filed tasks and ad-hoc runs, 20 for
trigger runs, 15 for relays; override per task with `--ceiling`.

## Code tasks

```bash
impyard imp task add yuko --repo ~/projects/site "fix the RSS date bug"
```

The dispatcher provisions a writable git worktree of that repo at `--base`
(default `main`) and mounts it into the box. The imp edits and then calls
`propose_changes` — a gated action whose diff rides on the gate for review.
On approval, the trusted side commits, pushes an `imp/<name>/…` branch, and
opens the pull request. The box never holds the push credential.

## Reorganization tasks

```bash
impyard imp task add yuko --reorganize "rebuild the topic index"
```

Takes the imp's exclusive knowledge-reorganization lease: the run may
rebuild `organization/` in the knowledge repo while ordinary append runs
continue alongside. One reorganization at a time per imp; see
[knowledge.md](knowledge.md).

## Runs: the permanent record

Every session — queued, ad-hoc, or chat — is a **run** with a durable
record. Run ids are timestamps with a random suffix
(`2026-07-10-21-51-17-a3b3`); any unique prefix works in commands.

```bash
impyard server runs ls --imp yuko
impyard server runs show 2026-07-10       # transcript, journal, knowledge
                                          # commits, files, how it ended
impyard server runs context <run>         # the exact compiled prompts
impyard server runs recall <run>          # why each memory was included
```

A run record answers how the session ended (`done`, `ceiling`, `error`,
`idle`), what knowledge mode it had and what commit it produced, what it
proposed, and what everything cost. The per-imp journal threads the story
across runs: what the box did, what was gated, who decided, what the
executor reported. Nothing here is model-written; it's all computed from
what actually happened.
