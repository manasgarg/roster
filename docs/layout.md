# On-disk layout (v3 — XDG, 2026-07-12)

**Status: implemented; migration is manual (admin-run, below).** Roster is a
general-purpose control plane: the code checkout contains **no config and no
state**, and there is **no deploy step** — config is read live from disk,
validated on every load, and a broken edit fails closed (the gateway denies,
dispatch pauses, `server run` refuses to boot). Every path is minted in
`src/paths.rs`.

The deployment follows the XDG Base Directory standard:

```
$XDG_CONFIG_HOME/roster/        ~/.config/roster — hand-edited, never machine-written
  org.toml                      grants, actions, trust, budgets, [engine] dir
  providers.toml                optional overlay on binary-shipped provider defaults
  workers/<name>/               worker.toml + identity.md

$XDG_DATA_HOME/roster/          ~/.local/share/roster — durable; THE BACKUP SET
  vault/                        credentials (0600), injected in transit
  ca/                           the TLS-interception keypair
  workers/<name>/               one worker's whole footprint (export = this + its spec):
    queue/  journal/  gates/{pending,resolved}/
    memory.jsonl  notes-legacy.jsonl  knowledge/repo.git
  channels/<id>/                trust designation, settings, purpose, history
  audit/                        append-only forever (invariant 9):
    decisions.jsonl  usage.jsonl  credentials.jsonl  messages.jsonl

$XDG_STATE_HOME/roster/         ~/.local/state/roster — reconstructible/prunable
  runs/<run-id>/                transcripts + workspaces (prunable with care)
  identity/                     ephemeral per-run box tokens
  locks/<worker>.lock           listener locks
  outbox/                       offline email sink (ROSTER_EMAIL_SINK testing)
  trigger-state.json            durable schedule cursors
```

Resolution order: `ROSTER_ROOT` (self-contained mode: everything under
`$ROSTER_ROOT/{config,data,state}` — tests, scratch deployments, side-by-side
instances) → `XDG_*_HOME` env vars → the XDG defaults. `ROSTER_VAULT_DIR` /
`ROSTER_CA_DIR` still override their spots individually.

Design notes:

- **No compile step.** Consumers parse the TOML straight into the gateway's
  own types via `src/config.rs` (`snapshot()`, mtime-cached, so edits are
  live). `roster server validate` runs the same loader and prints every error.
- **The box mounts none of this.** Isolation by absence, not by shadow
  mounts: the container sees only the engine checkout (`[engine] dir`,
  read-only), its own run dir, its channel history (read-only), and the CA
  certificate. The old `STATE_DIRS` shadow machinery and the `.env` canary
  are retired.
- **Worker-first data**: a worker's whole footprint is one subtree under
  `data/workers/<name>/`. Runs stay global (`state/runs/`) because run ids
  are cross-worker handles; a run's attribution lives in its `run.json`.
- Backup story: **config + data**; everything under state can burn.
- `roster init` creates all three roots (idempotent, never overwrites);
  `roster worker init <name>` scaffolds a worker into config and initializes
  its knowledge repo in data.

## Migrating the existing deployment

One-time, with the old daemon **stopped**. From the repo root — this list
matches the deployment as of 2026-07-12 (workers kdemo, knowledge-demo, yuko;
all gates yuko's; memory still in legacy notes/):

```bash
CFG=~/.config/roster; DATA=~/.local/share/roster; STATE=~/.local/state/roster

# config
mkdir -p "$CFG"
mv org.toml "$CFG/org.toml"          # already carries [engine] dir
mv workers "$CFG/workers"
rm providers.json                     # defaults now ship inside the binary
rm .env                               # the shadow canary is obsolete

# data
mkdir -p "$DATA/workers"/{kdemo,knowledge-demo,yuko} "$DATA/channels" "$DATA/audit"
mv ~/.roster/vault "$DATA/vault"
mv ~/.roster/ca "$DATA/ca"
mv runs/decisions.jsonl runs/usage.jsonl runs/credentials.jsonl "$DATA/audit/"
mv queue/kdemo           "$DATA/workers/kdemo/queue"
mv queue/knowledge-demo  "$DATA/workers/knowledge-demo/queue"
mv queue/yuko            "$DATA/workers/yuko/queue"
mv journal/org/kdemo     "$DATA/workers/kdemo/journal"
mv journal/org/yuko      "$DATA/workers/yuko/journal"
mv notes/yuko.jsonl      "$DATA/workers/yuko/notes-legacy.jsonl"
mv knowledge/kdemo       "$DATA/workers/kdemo/knowledge"
mv knowledge/yuko        "$DATA/workers/yuko/knowledge"
mkdir -p "$DATA/workers/yuko/gates"
mv gates/pending         "$DATA/workers/yuko/gates/pending"
mv gates/resolved        "$DATA/workers/yuko/gates/resolved"
mv channels/*            "$DATA/channels/"

# state
mkdir -p "$STATE"
rm runs/gateway.jsonl                 # dead: written by the retired TS gateway
rm -r runs/compiled                   # no compiled config anymore
rmdir runs/listeners 2>/dev/null || true
mv runs "$STATE/runs"
mv queue/.trigger-state.json "$STATE/trigger-state.json"
mv ~/.roster/identity "$STATE/identity"

# empty shells
rmdir queue journal/org journal notes gates knowledge channels ~/.roster
```

Then `roster init` (fills in anything missing, e.g. `state/locks`),
`roster server validate`, and `roster server run`. If a future deployment has
gates from several workers, split `gates/{pending,resolved}/*.json` by each
file's `"worker"` field into the matching worker subtree.
