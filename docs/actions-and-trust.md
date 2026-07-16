# Actions, gates, and trust

Anything a worker does with real consequences — sending an email, posting a
message, opening a pull request, editing what it is — is an **action**. An
worker never performs an action itself. It *proposes* one, and the trusted side
decides: execute it now, or hold it for a human. This page covers that whole
path: the proposal, the approval desk, and the trust ladder that lets a worker
earn its way out of asking.

The spine in one line: **the worker proposes; a human (or an earned trust rule)
disposes; the trusted side executes.**

## How a proposal travels

Inside the box, action tools don't act — they submit a typed envelope to the
gateway's internal action host:

```json
{
  "intent":    "email-send",
  "payload":   { "to": ["team@ourco.com"], "subject": "…", "body": "…" },
  "rationale": "weekly digest requested by the lead",
  "run_id":    "…",
  "task_id":   "…"
}
```

The `intent` is a name the admin declared in config; the payload is the
exact content that would go out. The gateway resolves *which worker* is asking
from the run's identity token on the connection — never from the envelope —
so a box cannot claim another worker's identity or grant itself an intent.

Then, in order:

1. **No matching grant** for that intent, in scope, means a refusal. Intents
   are granted per worker (or org-wide) with `[[action]]` blocks in config; an
   ungrated intent doesn't gate, it fails.
2. **Trust is evaluated** for this worker, intent, and payload (below).
3. **`auto`** — the executor runs immediately, and the box gets the result
   in the same call, like any API.
4. **`gate`** — the payload is frozen into a durable gate, and the box gets
   `pending` back right away. A gated action never blocks the container:
   boxes are ephemeral and humans are slow, so the wait happens in the queue,
   not in a held-open process.

Every disposition — refused, auto-executed, gated, approved, denied, failed —
is appended to the permanent audit log.

## The approval desk

```bash
roster server approvals ls               # what's waiting
roster server approvals show g-8f3a      # the exact bytes that would go out
roster server approvals approve g-8f3a   # a human decides
roster server approvals deny g-8f3a "not this recipient"
```

A gate is a timestamped state machine:

```
pending → approved (by whom, when) → executing → executed (result) | failed
pending → denied (by whom, why)
```

The payload is frozen at propose time — the human approves exactly the bytes
that will be sent, and the worker cannot alter them afterward. `show` renders
what matters for review: identity gates show a current-vs-proposed diff,
purpose gates likewise, code gates show the worktree diff. Execution is
idempotent: `executed` is terminal, and a crash mid-execute resumes without
double-sending.

Gates can also be approved from chat: in a trusted Discord or Slack channel,
trusted participants act as the approval desk (see
[channels.md](channels.md)).

### The worker sees its own gate state

The box that filed a gate is usually gone before the human decides, so
visibility works across runs. Every actor appends to the worker's journal — the
box (what it proposed), the desk (approved or denied, by whom), the executor
(the result) — and each new run is briefed with open gates and anything
resolved since. During a run, read-only tools let the worker re-check its gates
and journal. So it never re-proposes what's already pending, and a
continuation run knows exactly what was approved and whether it finished.

This costs nothing in safety: the journal is a *view*. Enforcement reads
only the trusted-side gate store, which the box cannot write, so a
compromised box scribbling "approved" into its own journal moves nothing.

If a grant sets `wake_on_resolve`, resolving the gate files a continuation
task automatically — a fresh box picks up with the outcome in its briefing.

## The trust ladder

Trust is per **(worker, intent)** and starts at the bottom: absent any rule,
every action gates. Three levels exist:

- **`gate`** — the birth state. A human approves every one.
- **`auto`** — executes immediately. Granted explicitly by the admin, or
  earned.
- **`earned`** — automatic once the track record justifies it: after N
  approved-and-executed gates (default 5) with **zero denials**, the action
  runs as `auto`. A single denial revokes the privilege and it gates again.

Rules live in config and can be narrowed by payload:

```toml
[[trust]]
intent = "email-send"
match  = { to = "*@ourco.com" }   # internal recipients only
level  = "earned"
after  = 10
```

Payload predicates are globs, and every field must hold. A list field (like
`to`) matches only if *all* its entries match — one external recipient in an
otherwise-internal email is enough to fall back to a gate.

Trust never lives in the worker's spec, and a worker can never raise its own
level: the ladder is derived from the gate history plus admin rules, both on
the trusted side. `roster worker trust <name>` shows the current state —
what's granted, what's earned, what would gate.

Three intents get special handling, hardwired:

- **Identity edits always gate.** No trust rule can promote them: what an
  worker *is* changes only with a human's approval. This closes the
  prompt-injection route to self-programming.
- **Channel sends and purpose edits auto-execute in trusted channels.** In a
  channel the admin has marked trusted, replies and purpose refinements flow
  without a gate — the channel's own participants could do these directly
  anyway. In untrusted channels they gate.
- **Memory notes follow the memory policy**, a parallel ladder based on
  scope, basis, and channel settings — see [memory.md](memory.md).

## The executors

Executors are trusted-side code holding real credentials the box never
sees. The admin binds intents to executors in config; the built-in set:

| Executor | What it does |
|---|---|
| `message-user` | Deliver a message to the lead: Discord DM, else Slack DM, else the local inbox (`audit/messages.jsonl`) |
| `discord` | Post to a Discord channel with the bot token from the vault |
| `slack` | Post to a Slack channel with the bot token from the vault |
| `email` | Send via SMTP over TLS with the `smtp` vault credential |
| `git-pr` | Commit the run's worktree, push an `worker/<name>/…` branch, open the pull request |
| `identity` | Overwrite the worker's identity file (always human-gated) |
| `purpose` | Overwrite a channel's purpose file |
| `task` | File a task on the worker's own queue (`file_task` — the bridge from conversations to clean-room research) |
| `note` | Interaction-memory operations: remember, forget, correct, pin, and the rest |

And the box-side tools that propose through them: `message_user`,
`discord_send`, `slack_send`, `send_email`, `propose_changes` (one intent
covering commit, push, and PR), `propose_purpose_edit`, `file_task`, and the
memory tools. Task runs also carry `task_complete`/`task_fail` — not granted
actions but part of the task protocol: the worker's outcome report, recorded
as evidence for the host's attestation (see [work.md](work.md)).

Because the box holds no write credential and has no egress grant to any
write host, it *cannot* perform these actions — only propose them. A fully
compromised box yields, at worst, spurious proposals, every one held at the
desk.
