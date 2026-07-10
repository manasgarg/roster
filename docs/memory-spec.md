# Memory — per-channel notes, recalled in context (spec)

**Status: spec — not yet implemented.** Realizes handoff §3.4, refined: memory is
what the worker has **learned in a channel**, scoped **per channel** (a DM is a
per-person channel, so this covers people too). It sits alongside purpose in the
channel store, is distinct from purpose, and does **not** touch identity. Builds
on the identity/purpose/gate machinery already shipped.

## The three tiers (and where memory fits)

- **Identity** (`identity.md`) — the worker's **constitution**: who it is, its
  standing rules. Owner-authored, changed **rarely and only by an admin**, never
  by the worker. Sacred, high bar. *Not* a target for learning.
- **Purpose** (`channels/<id>/purpose.md`) — its **assigned role** in a channel,
  set *for* it by trusted humans. **Directive** — "what should I do here?"
- **Memory** (`channels/<id>/notes.jsonl`) — what it has **learned** in this
  channel, accumulated *by* it from experience. **Descriptive** — "what do I know
  about this place and the people in it?"

Memory ≠ purpose: purpose is a role a human assigns; memory is facts the worker
observes. Both are per-channel and co-located, but one is prescriptive and one is
learned. Memory ≠ identity: identity is the fixed constitution; **learning goes
into memory, never into identity.**

## Goal (concrete)

yuko remembers per channel. What it learns talking to you in your DM stays in that
DM's memory; what it learns in #eng stays in #eng. It carries context forward
within a channel without bleeding it across channels.

```
(in your DM) you: "keep replies to a sentence or two"
yuko: (remembers, in this channel) "owner prefers terse replies"
… next time in this DM, unprompted, it's terse — but #eng is unaffected.
```

## Scope: per channel

- Memory is keyed by **channel** — `channels/<channel_id>/notes.jsonl`, beside that
  channel's `purpose.md` and `messages.jsonl`. A **DM channel** is 1:1, so its
  memory is effectively memory about that person; a **group channel**'s memory is
  about that channel's context and the people in it.
- **Recall is per channel.** A run for a channel conversation is given **that
  channel's notes** — never another channel's. So memory can't leak across
  channels.

## Notes

- Each entry `{id, ts, author?, note}` in `channels/<channel_id>/notes.jsonl`
  (append-only, gitignored runtime state, owner-visible/prunable). The box's repo
  mount is read-only, so the worker never writes here directly.
- **`remember(channel_id, note)`** — a box tool → a `remember` action, executor
  `note`, **trust auto** (jotting is low-stakes, advisory). The worker passes its
  current `channel_id` (as it does for `discord_send`). The trusted-side executor
  appends the note; journaled/audited like any action.
- **`forget(channel_id, note_id)`** — a box tool → a `forget` action (auto).
- **CLI**: `roster notes ls <channel_id> | rm <channel_id> <note_id>` for the owner
  to review and prune a channel's memory.

## Recall into runs

The worker's context = **Identity → (this channel's Memory) → Purpose → recent
conversation → the new message(s)**.

- **Session** (`session_system_prompt`): the channel's notes are included
  alongside its purpose — so a warm session carries the channel's learned context.
- **Non-channel tasks** (scheduled, code) have no channel and so no channel memory
  in v1 (a general/worker-wide bucket could be added later if needed).
- **Bounded**: v1 includes all of a channel's notes (kept small by pruning); cap or
  rank later if one channel's memory grows large.

## Identity stays sacred

There is **no promotion path from a note into identity**. Identity is edited only
by an admin, deliberately (a direct file edit or a heavyweight owner action) — the
worker has no tool to change it. The worker's `propose_identity_edit` tool is
**removed**; the `identity-edit` action remains for the admin/owner path only.

## Security invariants

- Memory is **advisory** — fed to the model, never an enforcement input (like the
  journal). Capabilities stay in grants + the gateway; a note can shape behavior
  but **cannot grant a capability or lift a gate**.
- Memory is written only via a **governed action** (auto); the box **cannot write
  the notes file** (read-only mount). The owner can review/prune per channel.
- **Per-channel isolation** — a note is recalled only in the channel it was filed
  in, so it can't leak into an unrelated conversation.
- **Identity is near-immutable** — owner/admin only, never the worker. With no
  note→identity promotion, there is **no self-programming path** at all: the worst
  an injected note can do is persist and mislead future runs in that one channel,
  and it's always prunable.

## Build order (small increments)

1. **Notes core** — `remember(channel_id, note)` action + `note` executor +
   `channels/<id>/notes.jsonl` + `roster notes ls|rm` + recall into the session
   system prompt. Remove the worker's `propose_identity_edit`.
2. **`forget` + recall tuning** — the `forget` action; capping/ranking per channel
   if memory grows; optional general/worker-wide bucket for non-channel tasks.

## Open decisions (recommended defaults)

- **Per channel** (matches purpose + history; DMs cover per-person). A separate
  worker-wide/general bucket is deferred.
- **`remember` is auto** (low-stakes, advisory). Identity — the only high-stakes
  self — is owner-only, so nothing the worker does here needs a gate.
- **Recall = all of the channel's notes** for v1; rank/cap later.
