# Memory — notes & promotion (spec)

**Status: spec — not yet implemented.** Realizes handoff §3.4 and D10 (the
promotion rule): workers append notes freely; only a gated step promotes a note
into always-loaded core; this closes the injection→self-programming hole. Builds
on the identity + gate machinery already shipped.

## Goal (concrete)

yuko remembers across sessions. It jots what it learns — your preferences, facts,
procedures — and those notes lead every future run. The important, durable ones
get promoted into its **core identity** (through the existing hard gate). So it
gets better over time instead of starting fresh each session.

```
you: "always keep replies to a couple of sentences"
yuko: (jots a note) "owner prefers terse replies — 1–2 sentences"
… next session, unprompted, yuko is terse.
… later, yuko proposes folding that into its identity → you approve → it's core.
```

## The two tiers (D10)

- **Notes** — free-form, append-only, **low-stakes** memory the worker jots
  freely. Fed into every run. **Advisory, never enforcement**: a note can shape
  behavior but cannot grant a capability or lift a gate.
- **Core (identity)** — the always-loaded standing self. Editing it is
  **hard-gated** (`identity-edit`, already built). **Promotion** = a note
  graduates into core through that gate.

This is exactly D10: workers append notes freely; only a gated curator step
promotes into core. A malicious or injected note can *persist* and mislead a
future run, but it can't escalate (capabilities stay in grants + the gateway) and
it can't rewrite core without a human approving the exact text.

## Notes

- **Stored per worker**, off the box: `notes/<worker>.jsonl` (append-only,
  gitignored runtime state, owner-visible/prunable). The box's repo mount is
  read-only, so the worker never writes here directly.
- **`remember(note)`** — a box tool → a `remember` action, executor `note`,
  **trust auto** (jotting is low-stakes, D10). The trusted-side executor appends
  `{id, ts, note}` to `notes/<worker>.jsonl`; journaled/audited like any action.
- **`forget(note_id)`** — a box tool → a `forget` action (auto): the executor
  removes a note. Owners prune via the CLI.
- **Recall** — notes are fed into **every run**: a `[Memory]` section (after
  identity, before purpose/briefing/task) for one-shot runs, and inside the
  session system prompt for conversations.
- **CLI**: `roster notes ls|rm <id>` for the owner to review and prune.

## Promotion (note → core)

When a note is a durable standing rule rather than a passing fact, it graduates
into identity: the worker (or owner) uses **`propose_identity_edit`** (already
built, hard-gated) to fold the note into `identity.md`; a human approves the exact
new identity. Once in core, the note can be dropped. This is the D10 curator
step — a person decides what becomes permanent.

## Recall into runs

- **One-shot** (`run_box`): after identity — `Identity → Memory → Purpose →
  Briefing → Task`.
- **Session** (`session_system_prompt`): notes included alongside identity +
  purpose.
- **Bounded**: v1 includes all current notes (kept small by pruning/promotion). If
  they grow, later refinements can cap to the most recent or most relevant.

## Security invariants

- Notes are **advisory** (fed to the model, never an enforcement input) — like the
  journal. Capabilities stay in grants + the gateway; a note can't escalate.
- Notes are written only via a **governed action** (auto); the box **cannot write
  the notes file** (read-only mount). The owner can review/prune.
- Promotion to core (**identity**) stays **hard-gated** (D10) — only a human
  approves what becomes permanent.

## Build order (small increments)

1. **Notes core** — the `remember` action + `note` executor + `notes/<worker>.jsonl`
   store + `roster notes ls|rm` + recall into runs (one-shot and session). Promotion
   already works via `propose_identity_edit`.
2. **`forget` + recall tuning** — the `forget` action; capping/relevance if notes
   grow.

## Open decisions (recommended defaults)

- **`remember` is auto**, not gated (D10: workers append freely). Notes are
  advisory; core is the gated part.
- **Per-worker notes** for v1 (memory of the owner/preferences is worker-wide);
  per-channel memory can come later.
- **Recall = all current notes** for v1, managed by pruning + promotion; cap or
  rank later if they grow.
