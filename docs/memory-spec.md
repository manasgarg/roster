# Memory — per-person notes, contextually recalled (spec)

**Status: spec — not yet implemented.** Realizes handoff §3.4, refined: memory is
what the worker has **learned about the people and context it interacts with**,
scoped to *who* it's talking to. It is distinct from purpose and does **not**
touch identity. Builds on the identity/purpose/gate machinery already shipped.

## The three tiers (and where memory fits)

- **Identity** (`identity.md`) — the worker's **constitution**: who it is, its
  standing rules. Owner-authored, changed **rarely and only by an admin**, never
  by the worker. Sacred, high bar. *Not* a target for learning.
- **Purpose** (`purpose.md`, per channel) — its **assigned role** in a channel,
  set *for* it by trusted humans. **Directive** — "what should I do here?"
- **Memory** — what it has **learned** about people and context, accumulated *by*
  it from experience. **Descriptive** — "what do I know about who I'm talking to?"

Memory ≠ purpose: purpose is a role a human assigns; memory is facts the worker
observes. Memory ≠ identity: identity is the fixed constitution; memory grows and
is advisory. **Learning goes into memory, never into identity.**

## Goal (concrete)

yuko remembers the people it talks to. When you message it, it recalls what it has
learned about *you*; when someone else does, what it knows about *them*.

```
you: "keep replies to a sentence or two"
yuko: (remembers, about you) "prefers terse replies — 1–2 sentences"
… next time you talk to it, unprompted, it's terse — but it isn't terse with
   someone who likes detail.
```

## Scope: per-person + general

- Memory is keyed by **who** it's about: a Discord **user id** (a specific person)
  or **`general`** (facts about the world/procedures, not any one person).
- **Recall is contextual.** A run is triggered by a conversation with specific
  people; the worker is given the memory about **the active participant(s) + the
  general bucket** — not everyone it has ever met.

## Notes

- **Stored per worker**, off the box: `notes/<worker>.jsonl`, each entry
  `{id, ts, about, note}` where `about` is a user id or `"general"`. Append-only,
  gitignored runtime state, owner-visible/prunable. The box's repo mount is
  read-only, so the worker never writes here directly.
- **`remember(note, about?)`** — a box tool → a `remember` action, executor
  `note`, **trust auto** (jotting is low-stakes). `about` defaults to the person
  the worker is currently talking to, or `general`. The trusted-side executor
  appends the note; journaled/audited like any action.
- **`forget(note_id)`** — a box tool → a `forget` action (auto): remove a note.
- **CLI**: `roster notes ls [about] | rm <id>` for the owner to review and prune.

## Recall into runs

The worker's context = **Identity → (Memory about the active people + general) →
Purpose → Briefing → Task**.

- **Session** (`session_system_prompt`): include memory about the channel's
  participants + general.
- **One-shot** (`run_box`): include general memory (+ any subject the task names).
- **Bounded**: v1 includes all notes for the in-scope subjects (kept small by
  pruning); cap/rank later if a person's memory grows large.

## Identity stays sacred

There is **no promotion path from a note into identity**. Identity is edited only
by an admin, deliberately (a direct file edit or a heavyweight owner action) —
the worker has no tool to change it. The worker's `propose_identity_edit` tool is
**removed**; the `identity-edit` action remains for the admin/owner path only.

## Security invariants

- Memory is **advisory** — fed to the model, never an enforcement input (like the
  journal). Capabilities stay in grants + the gateway; a note can shape behavior
  but **cannot grant a capability or lift a gate**.
- Memory is written only via a **governed action** (auto); the box **cannot write
  the notes file** (read-only mount). The owner can review/prune.
- **Identity is near-immutable** — owner/admin only, never the worker. With no
  note→identity promotion, there is **no self-programming path** at all: the worst
  an injected note can do is persist and mislead a future run, bounded to the
  person it's filed under, and always prunable.
- A note about person A is only recalled when interacting with A — so a note
  can't leak into an unrelated conversation.

## Build order (small increments)

1. **Notes core** — `remember(note, about?)` action + `note` executor +
   `notes/<worker>.jsonl` + `roster notes ls|rm` + contextual recall (session +
   one-shot). Remove the worker's `propose_identity_edit`.
2. **`forget` + recall tuning** — the `forget` action; capping/ranking per subject
   if memory grows.

## Open decisions (recommended defaults)

- **Per-person keyed by user id** (stable) with a display-name hint for the owner;
  plus a `general` bucket. Per-channel memory can come later if needed.
- **`remember` is auto** (low-stakes, advisory). Identity — the only high-stakes
  self — is owner-only, so nothing the worker does needs a gate here.
- **Recall = all notes for the in-scope subjects** for v1; rank/cap later.
