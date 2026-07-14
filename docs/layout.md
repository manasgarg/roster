# On-disk layout (v3 — XDG, 2026-07-12)

**Status: implemented; migration is manual (admin-run, below).** Impyard is a
general-purpose control plane: the code checkout contains **no config and no
state**, and there is **no deploy step** — config is read live from disk,
validated on every load, and a broken edit fails closed (the gateway denies,
dispatch pauses, `server start` refuses to boot). Every path is minted in
`src/paths.rs`.

The deployment follows the XDG Base Directory standard:

```
$XDG_CONFIG_HOME/impyard/        ~/.config/impyard — hand-edited, never machine-written
  org.toml                      grants, actions, trust, budgets, [engine] dir
  providers.toml                optional overlay on binary-shipped provider defaults
  imps/<name>/               imp.toml + identity.md

$XDG_DATA_HOME/impyard/          ~/.local/share/impyard — durable; THE BACKUP SET
  vault/                        credentials (0600), injected in transit
  ca/                           the TLS-interception keypair
  imps/<name>/               one imp's whole footprint (export = this + its spec):
    queue/  journal/  gates/{pending,resolved}/
    memory.jsonl  notes-legacy.jsonl  knowledge/repo.git
  channels/<id>/                trust designation, settings, purpose, history
  audit/                        append-only forever (invariant 9):
    decisions.jsonl  usage.jsonl  credentials.jsonl  messages.jsonl

$XDG_STATE_HOME/impyard/         ~/.local/state/impyard — reconstructible/prunable
  runs/<run-id>/                transcripts + workspaces (prunable with care)
  identity/                     ephemeral per-run box tokens
  locks/<imp>.lock           listener locks
  outbox/                       offline email sink (IMPYARD_EMAIL_SINK testing)
  trigger-state.json            durable schedule cursors
```

Resolution order: `IMPYARD_ROOT` (self-contained mode: everything under
`$IMPYARD_ROOT/{config,data,state}` — tests, scratch deployments, side-by-side
instances) → `XDG_*_HOME` env vars → the XDG defaults. `IMPYARD_VAULT_DIR` /
`IMPYARD_CA_DIR` still override their spots individually.

Design notes:

- **No compile step.** Consumers parse the TOML straight into the gateway's
  own types via `src/config.rs` (`snapshot()`, mtime-cached, so edits are
  live). `impyard server validate` runs the same loader and prints every error.
- **The box mounts none of this.** Isolation by absence, not by shadow
  mounts: the container sees only its own run dir, its channel history
  (read-only), and the CA certificate + trust bundle. pi and the extensions
  are baked into the impyard-box image (`[engine] dir` is an optional dev
  override that mounts a checkout over them). The old `STATE_DIRS` shadow
  machinery and the `.env` canary are retired.
- **Imp-first data**: an imp's whole footprint is one subtree under
  `data/imps/<name>/`. Runs stay global (`state/runs/`) because run ids
  are cross-imp handles; a run's attribution lives in its `run.json`.
- Backup story: **config + data**; everything under state can burn.
- `impyard init` creates all three roots (idempotent, never overwrites);
  `impyard imp init <name>` scaffolds an imp into config and initializes
  its knowledge repo in data.

## Migrating the existing deployment

One-time, with the old daemon **stopped**. From the repo root — this list
matches the deployment as of 2026-07-12 (imps kdemo, knowledge-demo, yuko;
all gates yuko's; memory still in legacy notes/):

```bash
CFG=~/.config/impyard; DATA=~/.local/share/impyard; STATE=~/.local/state/impyard

# config
mkdir -p "$CFG"
mv org.toml "$CFG/org.toml"          # already carries [engine] dir
mv imps "$CFG/imps"
rm providers.json                     # defaults now ship inside the binary
rm .env                               # the shadow canary is obsolete

# data
mkdir -p "$DATA/imps"/{kdemo,knowledge-demo,yuko} "$DATA/channels" "$DATA/audit"
mv ~/.impyard/vault "$DATA/vault"
mv ~/.impyard/ca "$DATA/ca"
mv runs/decisions.jsonl runs/usage.jsonl runs/credentials.jsonl "$DATA/audit/"
mv queue/kdemo           "$DATA/imps/kdemo/queue"
mv queue/knowledge-demo  "$DATA/imps/knowledge-demo/queue"
mv queue/yuko            "$DATA/imps/yuko/queue"
mv journal/org/kdemo     "$DATA/imps/kdemo/journal"
mv journal/org/yuko      "$DATA/imps/yuko/journal"
mv notes/yuko.jsonl      "$DATA/imps/yuko/notes-legacy.jsonl"
mv knowledge/kdemo       "$DATA/imps/kdemo/knowledge"
mv knowledge/yuko        "$DATA/imps/yuko/knowledge"
mkdir -p "$DATA/imps/yuko/gates"
mv gates/pending         "$DATA/imps/yuko/gates/pending"
mv gates/resolved        "$DATA/imps/yuko/gates/resolved"
mv channels/*            "$DATA/channels/"

# state
mkdir -p "$STATE"
rm runs/gateway.jsonl                 # dead: written by the retired TS gateway
rm -r runs/compiled                   # no compiled config anymore
rmdir runs/listeners 2>/dev/null || true
mv runs "$STATE/runs"
mv queue/.trigger-state.json "$STATE/trigger-state.json"
mv ~/.impyard/identity "$STATE/identity"

# empty shells
rmdir queue journal/org journal notes gates knowledge channels ~/.impyard
```

Then `impyard init` (fills in anything missing, e.g. `state/locks`),
`impyard server validate`, and `impyard server start`. If a future deployment has
gates from several imps, split `gates/{pending,resolved}/*.json` by each
file's `"imp"` field into the matching imp subtree.
