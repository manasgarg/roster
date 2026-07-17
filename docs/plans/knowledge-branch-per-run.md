# Plan: branch-per-run knowledge — full agency, host-owned main

Status: implemented (2026-07-17) — WP1–WP4 landed; needs a daemon restart and
a rebuilt box image (or an `[engine] dir` dev mount) to take effect
Scope: replace the knowledge mode system (append / reorganization / read,
namespaces, lease) with one workflow: every run works on a real git branch
and lands it through a gated push. The clean-room taint boundary is
untouched. Motivating incident: run `2026-07-17-01-46-51-94b26609` imported
434 files correctly, deleted one test record the task asked it to remove,
and lost the entire checkpoint to a silent post-exit append-mode rejection.

## Goal

The workflow every coding agent already knows, against a protected branch:

```
# inside the box, at /opt/roster/knowledge (a real clone, branch run/<id>)
git add -A && git commit -m "import research-kb"
knowledge_push                        # gated action → host validates, advances main
# refused with "main moved to <sha>"?
git fetch origin && git rebase origin/main   # resolve conflicts with full agency
knowledge_push                        # again
```

The host stays the **sole writer of `main`**: a box can never rewrite
history, so `git log` remains the audit trail and every landed change is
`git revert`-able. All conflict resolution happens in the box, by the
agent — the host only ever fast-forwards.

## Decisions (locked)

1. **Free-for-all tree.** Any clean run may add, edit, delete, and
   restructure anything. One guardrail: a push whose diff deletes more than
   N files (default 20, `[knowledge] max_deletions_ungated`) requires a
   human gate — history makes deletion recoverable, but a quiet bulk wipe
   deserves a speed bump.
2. **Exit backstop.** A run that ends with unpushed work (crash, ceiling,
   forgot) gets its worktree snapshotted host-side onto a
   `quarantine/run-<id>` ref in the canonical repo; the next run's briefing
   says so. Replaces today's quarantine directory. Pruned after 14 days.
3. **No `records/` / `organization/` split — at all.** No enforcement, no
   convention, no prompt text. The agent owns the layout. Existing repos
   keep their inherited layout as ordinary files the agent may now reshape.

## What the redesign deletes

| Today | Where | Fate |
|---|---|---|
| `KnowledgeMode` append/reorganization split | `src/worker/knowledge.rs:20` | deleted — one writable mode + read |
| Record namespaces (`ROSTER_RECORD_NAMESPACE`, filename suffix rule) | `knowledge.rs` provision, `boxed.rs:903` | deleted — branches make collisions resolvable instead of forbidden |
| Reorganization lease (`integrate.lock`, `acquire_lease` `knowledge.rs:965`) | worker data dir | deleted — the integration lane serializes pushes |
| Append/reorg validators (`validate_append :571`, `validate_reorganization :609`, `durable_paths_unchanged :648`) | `knowledge.rs` | deleted — replaced by the push-gate checks below |
| `--reorganize` task flag / `knowledge_mode` plumbing | `src/work/dispatch.rs:148`, `boxed.rs:93,160,306,702` | deleted |
| Post-exit auto-checkpoint (`finalize_storage` `boxed.rs:960`) | `boxed.rs` | becomes the backstop snapshot only — never auto-integrates |
| "The knowledge shelf" prompt section (modes, namespace ritual) | `src/worker/context.rs` `runtime_policy()` | rewritten: clone, commit, push, rebase-on-refusal |

What survives unchanged: the per-worker bare repo as the canonical store
and backup set, the serialized per-worker integration lane, the clean-room
taint rule (tainted run → read-only, `provision` `knowledge.rs:189`), the
journal events (renamed for pushes), `roster server runs show` reporting.

## Topology

- **Canonical**: `data/workers/<name>/knowledge/repo.git`, host-owned,
  `gc.auto=0` (host gc's explicitly in the lane when no runs are live).
- **Per run**: host clones (full history) into `runs/<id>/knowledge`,
  branch `run/<id>` at current `main`, commit identity set to the worker.
  Mounted rw for clean runs, ro for tainted — as today (`boxed.rs:832`).
- **Origin**: the bare repo is ALSO bind-mounted **read-only** at
  `/opt/roster/knowledge.git`, configured as `origin`. Bind mounts are live
  views, so after a refused push the box's `git fetch origin` sees the new
  `main` immediately. The ro mount makes ref writes from the box a
  filesystem error, not a policy hope.

## The push: a gated action carrying a bundle

`knowledge_push` rides the existing action framework (propose → trust/gate
→ execute host-side, `src/action/mod.rs:226`). The security-critical rule:
**the host never runs git against a box-writable repository.** A
box-written `.git/config` is an execution vector for any host git process
that touches it (`core.fsmonitor`, hooks, alternates). So the transfer is a
**git bundle** — an inert pack file:

1. Box side (action extension): verify the worktree is committed, run
   `git bundle create <run-dir>/push.bundle origin/main..run/<id>`, propose
   the action with the bundle path + head sha.
2. Host side: `git bundle verify`, then fetch **from the bundle** into a
   fresh quarantine repo with `transfer.maxObjectSize` / unpack limits set,
   `git fsck` the result. No upload-pack ever runs on box-written state.
3. Validate the `main..head` diff: secret scan (`obvious_secret`
   `knowledge.rs:898`), participant scan under `write_from = "any-run"`,
   path hygiene (no symlinks, no `.git*` paths, extension + size limits —
   `validate_relative_path :870`, `validate_text_file :840`), repo-size
   ceiling (`enforce_repo_size :672`), deletion count vs the gate threshold.
4. In the integration lane: advance `main` **fast-forward only**. If
   `merge-base(main, head) != main`, refuse with
   `{"status":"stale","main":"<sha>","hint":"fetch origin, rebase, push again"}`.
   The host never merges content; divergence is always resolved in the box.
5. Journal `knowledge-pushed` / `knowledge-push-refused` with the reason —
   the same events `runs show` and the next briefing read.

The refusal arrives as the action result, in-run — the agent can react.
The silent post-exit rejection class (the motivating incident) is
structurally gone: exits never integrate, only pushes do.

## Backstop and briefing

`finalize_storage` (`boxed.rs:960`) on any exit with a dirty or unpushed
worktree: snapshot the files (temp-tree hashing as today's checkpoint does
— never git-reading the box's `.git`) to `quarantine/run-<id>`, journal it,
and surface it in the worker's next briefing ("run <id> left unpushed
knowledge on quarantine/run-<id> — recover or discard"). Recovery is the
agent cherry-picking from the ref it can see via the ro origin mount.

## Non-goals

- Cross-worker shared knowledge repos, remote mirrors, redaction commands
  — same standing gaps as today (docs/knowledge.md "Current limitations").
- Host-side merge/conflict resolution. Fast-forward only, by design.
- A structured store. Files + git remain the substrate; that's the point.

## Risks, honestly

- **Rebase quality is the agent's.** A botched rebase can mangle files on
  the branch — but never `main`'s history; every push is revertable and the
  quarantine refs keep pre-push states. The mass-deletion gate catches the
  worst shape.
- **Bundle/unpack limits need real numbers.** A hostile box can emit a
  pathological pack; `transfer.maxObjectSize`, bundle size cap, and fsck
  run before anything touches the canonical repo.
- **Concurrent reader vs gc**: solved by `gc.auto=0` + lane-scheduled gc.
- **Model competence assumption**: the workflow is idiomatic for coding
  agents (the design premise). Warm-session workers on small models may
  fumble rebases; the backstop bounds the damage to "work parked on a ref".

## Work packages

- **WP1 — provisioning**: full clone + ro origin mount + `run/<id>` branch;
  drop modes/namespace/lease env and plumbing (`boxed.rs`, `knowledge.rs`,
  `dispatch.rs`).
- **WP2 — `knowledge_push`**: box extension (commit check, bundle create) +
  host executor (bundle verify/fetch/fsck, diff validation, ff-only advance
  in the lane) + journal events + gate wiring for the deletion threshold.
- **WP3 — backstop**: quarantine refs on exit, pruning, briefing surfacing.
- **WP4 — prompt + docs + tests**: rewrite `runtime_policy()` knowledge
  section and docs/knowledge.md; port the validator unit tests to the
  push-gate checks; end-to-end: concurrent runs, stale push → rebase →
  land, deletion gate, tainted refusal, backstop recovery.

WP1+WP2 land together (a clone without a push path strands work); WP3/WP4
can follow in the same release.
