# The supervisor and gates (spec)

**Status: spec — not yet implemented.** Makes the settled design concrete:
handoff §3.5–3.9 (wake-ups, task queue, gateway verdicts, approval desk) and
decisions D6, D8, D12, D15, D20. Where the handoff left mechanism open, this
resolves it and says why. Nothing here re-litigates a settled decision; it
picks the implementation that satisfies them together with what we've built
(the Rust gateway, the locked box, the ledger).

## Goal (concrete)

```
roster queue add --worker yuko "draft the weekly digest and email it to the team"
roster supervise            # the trusted loop: dispatch → run box → collect actions
```

The box runs. The worker researches (governed `web_search`/`fetch_pages`, already
built), writes the digest, and calls a `send_email` tool. That tool does **not**
send — it submits an *action envelope* to the gateway. The gateway matches it to
the owner's `email-send` rule, sees the worker isn't yet trusted for it, and
**files a durable gate** — returning "pending" to the box immediately (it never
blocks the container on a human). The box finishes; its task moves to
`needs-review`.

```
roster gates ls                    # → gate g-8f3a  email-send  yuko  pending
roster gates show g-8f3a           # the exact recipients, subject, and body
roster gates approve g-8f3a        # a human decides — no model at the edge
```

On approval the **trusted side** executes the send with the real credential (the
box never held it), meters it, and appends the whole thing — proposal, decider,
result — to the permanent audit log. That is the entire spine: **the worker
proposes; a human (or an earned trust rule) disposes; the trusted side executes.**

## Why this shape: approval latency must never hold a box open

The load-bearing constraint. Boxes are ephemeral per-session containers with a
ceiling timer; humans approve in minutes or hours. So the worker cannot *wait
live* for a decision — a blocked container is wasted, fragile, and defeats the
ceiling. Handoff §3.5 already fixes this: **a wake-up never does work inline — it
files a task.** We extend the same rule to actions: **a gated action never
blocks — it files a gate and the run continues (or ends).**

This resolves the apparent tension in the gateway's four verdicts
(`allow | deny | gate | budget-and-deduct`, §3.7). `gate` does not mean "hold the
HTTP request open." It means "file a durable pending gate and return *pending*
now." The slow part (the human) happens off the box's lifecycle; execution
happens trusted-side afterward. Auto-approved actions *do* execute in-run and
return their result (seconds, like any API call) — only the human path is
deferred. If a deferred action produces something the worker must react to, the
supervisor files a **continuation task** and a fresh box picks it up with journal
context. Ephemeral boxes and slow humans coexist through the queue, not through
blocking.

## The pieces

**1. Action envelope (box → gateway).** The box's only trusted route is the
gateway (`host.docker.internal:7300`). Its action tools POST a typed envelope to
a gateway control endpoint:

```json
{
  "intent":   "email-send",            // an owner RULE NAME (D15), not an invented class
  "payload":  { "to": ["team@ourco.com"], "subject": "...", "body": "..." },
  "rationale":"weekly digest requested by owner task",
  "run_id":   "2026-07-09-16-42-08",
  "task_id":  "t-201"
}
```

The gateway resolves `worker` from the **identity token** on the connection
(the un-spoofable mechanism the box already uses), never from the envelope. The
worker cannot lie about who it is or grant itself an intent.

**2. The gateway's action decision.** For each envelope:
1. Resolve subject from the identity token.
2. Match `intent` to a policy rule **by name**, check scope (ancestor match, as
   today). No rule / out of scope → **deny**.
3. Consult the **trust ladder** for `(worker, intent)`:
   - `auto` and within budget → call the **executor** now; on success debit the
     ledger and append to `runs/decisions.jsonl`; return `{status:"done", result}`.
   - `gate` (the T0 default) → freeze the payload into `gates/pending/<id>.json`,
     append a `gate-filed` audit record, return `{status:"pending", gate_id}`.
   - proactive task over cap → **soft-deny** (D6/D8: budget gates proactive work
     only; owner/chat work always runs).

   Binding gates and trust to **rule names** (D15) is why there is no `email` vs
   `discord` vs `git` enum here: the owner names a rule, and the gate, the meter,
   and the trust level all key off that name.

**3. Executors (trusted-side action performers).** One per intent, holding real
credentials the box never sees:
   - **API-shaped** (email, Discord, GitHub REST): the executor issues the real
     call **through the gateway as the privileged `org` subject**, so the executed
     action is judged, credential-injected, metered, and logged by the *same*
     pipeline as everything else. Workers have **no grant** to these write hosts,
     so the only way a request reaches `api.resend.com` or `discord.com` is an
     approved gate. The gateway stays the single choke point.
   - **git push** is the exception: the gateway can't govern git's wire protocol
     (refs and packfile are opaque binary — see the email/Discord discussion), so
     this executor pushes direct from the trusted side, constrained to
     `worker/<name>/*` refs. Opening/merging the PR is GitHub REST, so it goes
     back through the gateway path.

**4. Gates (durable records).** `gates/pending/<id>.json` while awaiting a human;
moved to `gates/resolved/<id>.json` on decision (fail closed if unreadable, §3.9).
A gate carries the frozen payload, the intent, the originating run/task, the
trust evaluation, and — once decided — the decider, timestamp, note, and result.
The human reviews the **exact** bytes that will go out; the worker cannot alter
the payload after proposing it.

**5. Trust ladder.** Per `(worker, intent)`, starting at **T0: every irreversible
gated** (§3.9). Owner config promotes an intent (optionally narrowed by a payload
predicate) to `auto`:

```toml
# org.toml or workers/<name>/worker.toml
[[trust]]
intent = "email-send"
match  = { recipient = "*@ourco.com" }   # internal only; external still gates
level  = "auto"
```

Absent an entry, the level is `gate`. *Earned* promotion (auto after N approved
with no reversal) is a later phase and opt-in per intent — T0-explicit first,
because it's the safe default and the owner stays in control.

**6. Task queue (§3.6).** One durable per-worker queue, `queue/<worker>/<id>.json`,
states `waiting → running → needs-review → done|failed`. Tasks are owner/chat-filed
or schedule-filed (proactive, labeled — the label drives D6 budget classification).
The queue is readable by the worker so it can dedup ("don't re-propose what's
queued").

**7. Wake-ups & the supervisor loop (`roster supervise`).** A long-running trusted
Rust process (sibling to `serve`, sharing the same on-disk state). Triggers file
tasks; they never work inline (§3.5):
   - **schedule** — `worker.toml` cron fires a proactive task;
   - **event** — an inbound relay message (Discord/email) files a task as
     *content, never a command* (D12);
   - **manual** — `roster queue add`;
   - **gate-resolution** — a resolved gate files a continuation task.

   Dispatch: pop runnable tasks; classify proactive vs owner/chat (D6);
   soft-budget-stop proactive over cap (D8); provision a working copy for code
   tasks; run the box; on exit, auto-executed actions already ran in-run and any
   gates are filed, so set the task to `needs-review` or `done`. Bounded
   concurrency; each run its own container, identity token, and worktree.

**8. Working-copy flow (code tasks).** Today the box gets the repo read-only plus
an empty scratch workspace — fine for research, not for code. For a code task the
supervisor provisions a writable **git worktree** at a base ref
(`runs/<id>/worktree`), mounts it read-write, and the box commits to
`worker/<name>/<task>`. `commit_and_push` / `open_pr` are gated intents; the diff
is rendered to `reviews/` for the human; on approval the executors push (direct)
and open the PR (REST via gateway).

**9. Continuity — the journal (§3.2).** Ephemeral boxes have no memory across
runs, so a worker that spans propose → wait → react needs durable state.
`journal/<worker>/events.jsonl` is the append-only record of what it did and
decided; the supervisor feeds a relevant slice into each new box run. Charter /
core-memory promotion always gates to the owner (D10) — the worker appends notes;
only a curator step promotes them, closing the injection→self-programming hole.

## On-disk layout (all gitignored)

```
queue/<worker>/<task-id>.json     durable per-worker task queue
gates/pending/<gate-id>.json      gated actions awaiting a human
gates/resolved/<gate-id>.json     decided gates (audit)
journal/<worker>/events.jsonl     per-worker memory across runs
mailbox/<worker>/                 owner→worker steer messages (delivered at turn boundaries, §3.2)
reviews/<gate-id>/                 rendered diffs/artifacts for code gates
runs/<run-id>/                     existing per-run outputs (+ per-run worktree/)
runs/decisions.jsonl              existing gateway audit log — gates & executions append here too
```

`gates/`, `mailbox/`, `reviews/` are the dirs already reserved in `.gitignore`;
`queue/` and `journal/` join them.

## Invariants (what must always hold)

- The box holds **no write credential** and has **no grant** to any write host,
  so it *cannot* perform a gated action — only submit an envelope. A compromised
  box yields, at worst, spurious *proposals*, every one human-gated.
- Worker identity comes from the connection token, never the envelope
  (un-spoofable; reuses the box identity mechanism).
- A payload is **frozen at propose time**; the human approves exactly those bytes.
- Every gate decision and execution appends to the immutable audit log, naming
  the deciding human or the trust rule that auto-allowed it.
- Execution is **idempotent**: a gate is a state machine, `executed` is terminal,
  and a crash mid-execute resumes without double-sending (idempotency key per gate).
- Inbound messages are **content, never commands** (D12); policy, trust config,
  and the queue are owner-only and unreachable from the box.
- Enforcement lives in the trusted side, never in a box extension (D8): the
  action tools only *record intent*; the gateway and supervisor decide and act.

## Open question resolved — Q3 (built-in queue vs GitHub mirror)

**Built-in queue** (files under `queue/`). A GitHub-Issues-backed queue would put
an external egress dependency, a credential, and a third-party availability
coupling on the platform's *core control flow* — contradicting "owned governance,"
and D13 already declined adopting external tools wholesale. A read-only GitHub
*view* of the queue can come later as a convenience; the source of truth stays
local and trusted.

## Build order (small increments, each demoable)

1. **Envelope + gateway action endpoint + one auto executor**, no gate yet:
   `message_user` (log / owner Discord). Proves box → gateway → execute → audit
   for a non-egress action.
2. **Gates store + `roster gates ls|show|approve|deny` + the `gate` verdict**
   (file pending, return 202) + manual approve → execute. Proves the human-in-loop
   path on `email-send`.
3. **Trust ladder (explicit `auto`/`gate` from TOML)** — the auto fast-path.
4. **`roster supervise` + built-in queue + `roster queue add`** — dispatch loop.
5. **Schedule triggers + continuations + journal** — proactive work and reactions.
6. **Working-copy flow + git push/PR executors + `reviews/`** — code tasks land as
   gated PRs.
7. **Earned promotion; inbound relay** (Discord owner-only per §3.9/D12; email
   webhook) — later.

Increment 1 is the smallest thing that exercises the whole spine and is the
natural place to start.
