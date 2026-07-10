# The worker charter (spec)

**Status: implemented and verified live, 2026-07-10.** Realizes handoff §3.1 (charter) and D10
(charter edits always gate to the owner; closes the injection→self-programming
hole), building on the box mount model and the gate machinery already shipped.

> **Superseded (design):** once a worker serves multiple Discord channels, the
> single `charter.md` splits into **`identity.md`** (fixed across channels) +
> per-channel **`purpose.md`** — see `docs/inbound-spec.md`. The mechanism below
> (read live, prepended to the run, hard-gated edits) carries over unchanged;
> only the file layout and composition change. `charter.md` remains the shipped
> form until that lands.

## Goal (concrete)

```
workers/yuko/charter.md      # owner-authored: who Yuko is, its job, its standing rules
roster queue add --worker yuko "draft the weekly digest"
```

Every run Yuko does — this task, a scheduled triage, a continuation — starts
with its charter as context, so behavior is coherent across runs instead of a
cold prompt each time. Yuko can *suggest* a change to its own charter, but only
the owner can apply it.

## What the charter is — and isn't

- **Is:** standing instructions to the model — role, responsibilities, standing
  rules, tone. The worker's "always-loaded core."
- **Isn't:** permissions. What a worker is *allowed* to do lives in owner config
  and the gateway — grants (`[[grant]]`), actions (`[[action]]`), the trust
  ladder, budgets. The charter shapes behavior *within* those bounds; it cannot
  grant a capability or lift a gate.

**This separation is the safety story.** Feeding freeform, possibly
injection-tainted text into the worker is safe because the charter can only
influence what the model *tries*, never what the gateway *allows*. A charter
that says "email anyone freely" still hits the same default-deny judge and
`email-send` gate. Behavior is advisory; enforcement is not.

## Where it lives

`workers/<name>/charter.md` — owner-only, versioned in the repo, and mounted
**read-only** into the box (the box gets the whole repo read-only). So the
worker can read its charter but cannot rewrite it. (An optional org-wide
`charter.md` shared by every worker is a later layer — see build order.)

## Fed into every run

`run_box` reads the charter host-side and **prepends it to the prompt**, so it
applies to both the ad-hoc `roster box` path and supervised runs, ahead of the
briefing and the task:

```
[Charter]     your standing role and rules (this run can't change them)
[Briefing]    open gates / continuation outcome   (supervised runs only)
[Task]        the actual task
```

Read **live** on each run — it's content, not compiled config, so an owner edit
takes effect on the next run with no `roster deploy`.

## Editing it — always gated (D10)

Two paths, both owner-controlled:

- **Owner edits `charter.md` directly.** It's their file; next run picks it up.
- **The worker proposes a change.** A `propose_charter_edit(charter, rationale)`
  tool submits the proposed **full** new charter as a `charter-edit` action. This
  action is **hard-gated**: the trust ladder does not apply to it (unlike other
  intents, it can never be promoted to auto — D10 says charter edits *always*
  gate). The gate carries the proposed text; `roster gates show` renders a diff
  (current vs proposed). On approval the `charter` executor writes
  `workers/<name>/charter.md` atomically; on denial nothing changes.

This closes the injection→self-programming hole: a prompt injection can make the
worker *propose* a new charter, but only the owner can apply it — and even an
applied malicious charter can't escalate capabilities (see the safety story).

## Scaffolding

`roster create <name>` writes a starter `charter.md` next to `worker.toml`, with
a template the owner fills in (role, responsibilities, standing rules).

## Invariants

- The charter shapes **behavior, never permissions**. Capabilities stay in owner
  config + the gateway.
- The box **cannot write its charter** (read-only mount); worker edits are
  owner-gated proposals.
- `charter-edit` **ignores the trust ladder** — it always gates (D10).
- The charter is read **live**; owner edits need no deploy.
- The proposed charter is **frozen** into the gate; the owner approves exactly
  the text that will be written.

## Build order (small increments)

1. **Charter file + feed + scaffold** — `workers/<name>/charter.md`, prepended to
   every run by `run_box`; `roster create` writes a template. (The foundational
   win; testable immediately.)
2. **The D10 edit loop** — `propose_charter_edit` tool, the hard-gated
   `charter-edit` action, the `charter` executor, and a current-vs-proposed diff
   in `roster gates show`.
3. **(Later)** an org-wide charter layer (shared standing rules, composed before
   the per-worker charter); and the memory/notes system (§3.4) whose promotion
   step is exactly an owner-gated charter edit.

## Open decisions (recommended defaults)

- **Full replacement, not a diff, for proposals.** The worker proposes the whole
  new charter; the owner reviews the complete result (with a rendered diff) and
  the executor overwrites. Simpler and unambiguous than applying a patch.
- **Org-wide charter deferred** to keep increment 1 minimal; the per-worker
  charter is the core.
