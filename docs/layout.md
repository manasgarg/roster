# On-disk layout

Roster is a control plane: the code checkout contains **no config and no
state**, and there is **no deploy step** — config is read live from disk,
validated on every load, and a broken edit fails closed (the gateway
denies, dispatch pauses, `server start` refuses to boot).

The deployment follows the XDG Base Directory standard:

```
$XDG_CONFIG_HOME/roster/       ~/.config/roster — hand-edited, never machine-written
  org.toml                      grants, actions, trust, budgets, policies
  providers.toml                optional overlay on the built-in provider registry
  connections/<name>.toml       service connections (wizard-scaffolded)
  workers/<name>/               worker.toml + identity.md

$XDG_DATA_HOME/roster/         ~/.local/share/roster — durable; THE BACKUP SET
  vault/                        credentials (0600), injected in transit
  ca/                           the TLS-interception keypair
  workers/<name>/               one worker's whole footprint:
    queue/  journal/  gates/{pending,resolved}/
    memory.jsonl  knowledge/repo.git
  channels/<id>/                trust designation, settings, purpose, history, files
  audit/                        append-only forever:
    decisions.jsonl  usage.jsonl  credentials.jsonl  messages.jsonl

$XDG_STATE_HOME/roster/        ~/.local/state/roster — reconstructible/prunable
  runs/<run-id>/                transcripts + workspaces + context traces
  identity/                     single-use per-run box tokens
  locks/                        listener locks
  outbox/                       offline email sink (ROSTER_EMAIL_SINK testing)
  trigger-state.json            durable schedule cursors
```

Resolution order: `ROSTER_ROOT` (self-contained mode: everything under
`$ROSTER_ROOT/{config,data,state}` — tests, scratch deployments,
side-by-side instances) → the `XDG_*_HOME` env vars → the XDG defaults.
`ROSTER_VAULT_DIR` / `ROSTER_CA_DIR` override their spots individually.

Design notes:

- **No compile step.** Consumers parse the TOML straight into the runtime's
  own types; the snapshot is mtime-cached, so edits are live.
  `roster server validate` runs the same loader and prints every error.
- **The box mounts none of this.** Isolation by absence: the container sees
  only its own run directory, its channel history (read-only), its
  knowledge checkout, and the CA certificate and trust bundle. pi and the
  extensions are baked into the roster-box image (`[engine] dir` is an
  optional dev override that mounts a checkout over them).
- **Worker-first data**: a worker's whole footprint is one subtree under
  `data/workers/<name>/` — export is that subtree plus its spec. Runs stay
  global (`state/runs/`) because run ids are cross-worker handles; a run's
  attribution lives in its own record.
- **Backup story: config + data.** Everything under state can burn.
- `roster init` creates all three roots (idempotent, never overwrites);
  `roster worker init <name>` scaffolds a worker into config and initializes
  its knowledge repo in data.
- Worth doing on day one: `git init ~/.config/roster` — your governance
  config is small, hand-edited, and exactly the kind of thing that deserves
  a reviewable history.
