# The CLI (v2, 2026-07-12)

**Status: implemented.** The `roster` binary's grammar is three groups that
mirror the product thesis — *rented intelligence, owned governance*:

- **`roster server …`** — the owned machinery: the daemon, config validation,
  the approval desk, channel edges, the credential vault.
- **`roster worker …`** — the governed identities: lifecycle, trust, memory,
  knowledge, and each worker's durable task queue.
- **`roster agent …`** — the rented intelligence: run a governed engine
  session directly, and inspect what past sessions did and saw.

Parsing is clap (derive). `roster help <cmd…>`, `-h/--help` on every node,
`-V/--version` prints the release plus the git build hash (so a stale running
daemon can be told apart from the binary you just built — ops scar §8.3 in the
handoff). Unknown `--flags` are always errors, never prompt text. IDs (tasks,
gates, runs) accept any unique prefix. `ls`/`show`/`status` commands take
`--json` for scripting.

## The tree

```
init              create the config/data/state roots (XDG; idempotent)

server run        [--cap N] [--once] [--no-listen] [--addr HOST:PORT]
server status     [--json]
server validate   parse + check all config, print every error
server gates      ls [--json] | show <id> | approve <id> [note] | deny <id> [note]
server channel    ls [--json] | show <id> | trust <id> | untrust <id>
                  | set <id> <key> <value>
                    keys: mode, memory, memory-inferred, memory-kinds,
                          memory-retention, memory-notes, memory-chars
server vault      connect <provider> | sync | ls [--json]

worker init       <name>
worker ls         [--json]
worker show       <name> [--json]
worker trust      <name> [--json]
worker task       add <worker> [--ceiling M] [--proactive|--reorganize]
                      [--repo P --base R] "<prompt>"
                  | relay <worker> [--from WHO] "<message>"
                  | ls [--json] | show <id> | requeue <id>
worker memory     ls <worker> [--scope S] [--scope-id ID] | show <worker> <id>
                  | correct <worker> <id> "<replacement>"
                  | rm|pin|unpin|disable|enable <worker> <id> | compact <worker>
worker knowledge  <name>

agent run         [-w WORKER] [--ceiling M] "<prompt>"
agent chat        <worker> [--idle SECS]
agent ls          [--worker W] [--limit N] [--json]
agent show        <run>
agent context     <run> [--all]
agent recall      <run>
```

## One daemon

`server run` merged the three pre-v2 daemons (`serve`, `supervise`, `listen`):
the gateway accept loop, the task-dispatch loop, and one Discord listener per
worker run as supervised siblings in a single process — one thing to start,
one thing to restart after a rebuild. Listeners restart with backoff on
disconnect or error and never take the gateway down with them.

A worker opts into a listener in its spec:

```toml
# workers/<name>/worker.toml
[channels]
discord = "discord"      # the vault credential its bot uses
```

Config validation fails if two workers claim the same credential (one bot
serving two workers would double-file every message). `--no-listen` runs gateway + dispatch only — the
sanctioned way to boot-test without double-connecting a live bot. `--once`
fires due triggers, drains due tasks, and exits.

## Old → new

Every pre-v2 command prints a pointer to its new home (exit 2) rather than
half-working — argument shapes changed (workers became positional, daemons
merged), so silent translation could misparse.

| old | new |
|---|---|
| `serve` / `supervise` / `listen --worker W` | `server run` |
| `deploy` | `server validate` (config loads live — no deploy step) |
| `gates …` | `server gates …` |
| `channel …` (incl. `memory-*` subcommands) | `server channel …` / `channel set` |
| `connect <p>` / `vault-sync` | `server vault connect <p>` / `server vault sync` |
| `create <n>` | `worker init <n>` |
| `queue add --worker W "p"` | `worker task add W "p"` |
| `relay --worker W "m"` | `worker task relay W "m"` |
| `memory <sub> --worker W` (and `notes`) | `worker memory <sub> W` |
| `memory explain <run>` | `agent recall <run>` |
| `knowledge W` | `worker knowledge W` |
| `box [--worker W] "p"` | `agent run [-w W] "p"` |
| `session --worker W` | `agent chat W` |
| `runs ls\|show\|context` | `agent ls\|show\|context` |

New in v2 (no old equivalent): `server status`, `server vault ls`,
`worker ls`, `worker show`, `worker trust`.

## The layout underneath

The deployment follows the XDG base dirs (config / data / state; `ROSTER_ROOT`
for a self-contained root) — see `docs/layout.md` for the full tree and the
migration steps. Config loads live through `src/config.rs`: no deploy step,
mtime-cached, fail-closed on errors. The box mounts no roster directories at
all — only the engine checkout (`[engine] dir` in org.toml), its own run dir,
channel history, and the CA cert. `worker task add`/`relay` validate the
worker exists before filing, and run ids carry a random suffix so concurrent
dispatch cannot collide on a run directory.
