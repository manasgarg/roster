# The memory/knowledge boundary (spec, 2026-07-14)

**Status: increments 1–3 implemented and verified live.** Taint is one
predicate (`RunContext::tainted()`: channel/user context, or `inbound` for
relay tasks whose prompt embeds third-party content). Tainted runs mount the
shelf `:ro` with `IMPYARD_KNOWLEDGE_MODE=read` and no namespace; clean runs get
no memory recall. `file_task` bridges; the participant scan polices it.

Verified in the live deployment (imp kdemo, relay tasks):

- Tainted run: `docker inspect` shows the knowledge mount `RW=false mode=ro`;
  run record `mode: "read", state: "read-only"`; the box reported `MODE=read`.
- The bridge, end to end: a tainted run called `file_task` → filed a
  imp-only task → that clean task ran with `mode: "append"` and integrated
  a commit (453b0dc). Person-space read the shelf; world-space wrote it.
- The deny branch is unit-tested at the exact call site
  (`boundary::check_task_prompt`), because the live imp declined to even
  attempt the refused calls — correct behavior, but not a test of enforcement.

Deferred as specced: receipts-for-records, `imp knowledge redact`.

**The problem.** Memory (person-space: consent, scopes, forget, retention) and
knowledge (world-space: immutable records, Git history, org-visible) have
different governance for good reason — and the leak is asymmetric. Knowledge
read in a conversation is harmless; conversation content written into
knowledge is a consent bypass into a store that is deliberately hard to erase.
No text filter closes this: paraphrase defeats every scanner. As long as one
model context holds both a conversation and a knowledge-write capability, the
boundary is advisory.

**The principle.** Make it an information-flow property, enforced by
provisioning, not prompts: **knowledge may only be written by runs that never
contained person-data.** Taint is a fact the host already knows about every
run; the model never chooses its own privilege — the run's provenance does.

## Taint and the two setups

A run is **tainted** when interaction content or context enters it:

- channel sessions (Discord/Slack, DM or channel) — always
- relay tasks (inbound message filed as a task; they carry channel/user
  context for reply routing) — always
- continuation runs inheriting a channel context
- any run with interaction-memory recall injected (memory is person-space
  by definition; imp-scoped notes included)

A run is **clean** when it is imp-only scope: trigger-fired, admin-filed
(`imp task add`), self-filed (below), or adhoc `imp run` — prompt only,
no recall, no participants.

| Run | Knowledge mount | Recall |
|---|---|---|
| tainted | **read-only** (`KnowledgeMode::Read`) | as today |
| clean | **read-write** (append, or reorganization) | none |

Read-only mechanics: the checkout mounts `:ro`, `IMPYARD_KNOWLEDGE_MODE=read`,
no record namespace, no checkpoint, run record notes `mode: "read"`.
Consulting the shelf mid-conversation is unchanged — world→person is the safe
direction.

## The bridge: self-filed tasks

Tainted runs that discover work worth durable research file a task instead of
writing records. New governed action:

```
file_task — executor "task"; payload { prompt, ceiling_min? }
```

The filed task is **imp-only by construction** (no channel/user context
attached — a channel id may appear in the prompt as a delivery *address*;
addresses are not content, and outbound sends stay governed by channel trust).
Default trust: auto for the imp's own queue, like `remember` — the
crossing is controlled by the scan below, and admins can ladder it to "gate"
per imp.

This makes the task prompt the **entire residual leak surface**: one
paragraph per crossing — journaled, scannable, gateable — instead of a wide
border of bulk file writes.

## The scan: participants, not PII

Generic PII detection is mushy; impyard has an unfair advantage — the host
knows exactly who was in the filing run. Scan two places:

1. **`file_task` payloads filed from tainted runs** (the choke point).
2. **New records at checkpoint** (defense-in-depth for `write_from =
   "any-run"` deployments, and against history already contaminated).

Match, exactly: the run's channel id, `user_id`, author ids and display names
from that channel's history; mechanically: Slack member ids (`U…`/`W…`),
Discord snowflakes in mention syntax, `<@…>`, email addresses. A hit **denies
with a legible reason** ("names a conversation participant — that belongs in
memory") and journals the event; the imp rephrases. Policy can escalate
deny → gate for human review.

Known limit, stated honestly: paraphrase still passes the scan. The scan
polices the choke point; the *hard* guarantee is the mount — a tainted run
cannot write records at all, scanned or not.

## Policy

```toml
[knowledge]
write_from = "clean-room"   # default; "any-run" = legacy behavior (scan-only)
```

Per-imp overlay as usual. Turning `clean-room` on for an existing
deployment should be paired with a one-time audit of existing records (a
leaked person-fact propagates into every future clean room via the base
clone). A later `imp knowledge redact` (audited git-filter surgery) is the
repair path — specced here, deferred.

## Imp-facing text

The knowledge-shelf paragraph of the runtime policy gains the rule and the
recipe: records describe the world, never the people you talk with — no
names, handles, ids, or quotes of participants (those belong in memory, where
people can see and manage them); in conversations the shelf is read-only —
use file_task to queue durable research for your next clean run. The
read-only case is also stated in the channel-session scope text.

## Reinforcement (separate increment): receipts for records

Fetch receipts (530e1c8) let the gateway attest what a run actually fetched.
A `[knowledge] require_receipts = true` mode would require records to cite
receipt ids for quoted external content — knowledge becomes "things the
world said, with the gateway as witness," and a clean room can only launder
what its task prompt smuggled in. Deferred until the boundary above is live.

## Increments

1. **`KnowledgeMode::Read` + provenance-based mounts** (M): mode plumbing in
   provision/boxed, taint derivation from RunContext, `write_from` knob,
   policy/scope text. Sessions and relay tasks flip to read-only.
2. **`file_task` action + executor + box tool** (S): queue::add reused;
   journal event per crossing.
3. **Participant scan** (M): shared matcher over (channel history authors +
   mechanical patterns); wired at file_task submit and checkpoint validate.
4. Later: receipts-for-records; audit pass + `imp knowledge redact`.

## Verification

- Session run (live channel): `IMPYARD_KNOWLEDGE_MODE=read`, write to the
  mount fails with EROFS, no checkpoint attempted, run record says read.
- Relay task: same.
- Trigger/admin task: writable, checkpoint commits as today.
- `file_task` from a session: task appears imp-only; prompt containing a
  participant's name is denied with the legible reason and journaled.
- Checkpoint of a record naming a run participant (any-run mode): quarantine
  with the same reason.
- Existing tests keep passing; new unit tests for taint derivation and the
  matcher.

## Invariants

The four-store model stays intact: identity (lead-owned), journal (append-only
record of what happened — task provenance belongs here, not in records),
memory (consent-governed person-space), knowledge (integrity-governed
world-space). This spec adds the fifth rule: crossings between person-space
and world-space are explicit, minimal, and host-inspected — provenance
governs the border.
