# The CLI (v2.1, 2026-07-13)

**Status: implemented.** The `impyard` binary's grammar is two groups that
mirror the product thesis — *rented intelligence, owned governance*:

- **`impyard server …`** — the owned machinery: the daemon, config validation,
  the approval desk, channel edges, the credential vault, and the run log
  (every session, whoever ran it).
- **`impyard imp …`** — the governed identities: lifecycle, trust, memory,
  knowledge, each imp's durable task queue — and running sessions as one,
  directly or interactively.

(v2 had a third group, `impyard agent …`. It folded into the other two on
2026-07-13: sessions belong to imps, the audit log to the server. Every run
is now attributed to a real, spec'd imp — the implicit `adhoc`
pseudo-imp is gone; make a scratch imp if you want one. `run` is always
the verb "execute now" — `server start` runs the daemon, `imp run` runs a
session — and `runs` is always the noun, the records.)

Parsing is clap (derive). `impyard help <cmd…>`, `-h/--help` on every node,
`-V/--version` prints the release plus the git build hash (so a stale running
daemon can be told apart from the binary you just built — ops scar §8.3 in the
handoff). Unknown `--flags` are always errors, never prompt text. IDs (tasks,
gates, runs) accept any unique prefix. `ls`/`show`/`status` commands take
`--json` for scripting.

## The tree

```
init              create the config/data/state roots (XDG; idempotent)

server start      [--cap N] [--once] [--no-listen] [--addr HOST:PORT]
server status     [--json]
server validate   parse + check all config, print every error
server connect    [<service>] [--imp W].. [--org] [--as NAME]
                    one-step service connection: login → vault → scaffold
                    connections/<name>.toml (bare: the catalog); docs/connections.md
server connections [--json]   the inventory: scope, hosts, env, active/disabled
server gates      ls [--json] | show <id> | approve <id> [note] | deny <id> [note]
server channel    ls [--json] | show <id> | trust <id> | untrust <id>
                  | set <id> <key> <value>
                    keys: mode, memory, memory-inferred, memory-kinds,
                          memory-retention, memory-notes, memory-chars
server vault      connect <provider> | sync | ls [--json]
server runs       ls [--imp W] [--limit N] [--json]
                  | show <run> | context <run> [--all] | recall <run>

imp init       <name>
imp ls         [--json]
imp show       <name> [--json]
imp trust      <name> [--json]
imp run        <name> [--ceiling M] "<prompt>"
imp chat       <name> [--idle SECS]
imp task       add <imp> [--ceiling M] [--proactive|--reorganize]
                      [--repo P --base R] "<prompt>"
                  | relay <imp> [--from WHO] "<message>"
                  | ls [--json] | show <id> | requeue <id>
imp memory     ls <imp> [--scope S] [--scope-id ID] | show <imp> <id>
                  | correct <imp> <id> "<replacement>"
                  | rm|pin|unpin|disable|enable <imp> <id> | compact <imp>
imp knowledge  <name>
```

## One daemon

`server start` (alias: `server run`) merged the three pre-v2 daemons
(`serve`, `supervise`, `listen`):
the gateway accept loop, the task-dispatch loop, and one Discord listener per
imp run as supervised siblings in a single process — one thing to start,
one thing to restart after a rebuild. Listeners restart with backoff on
disconnect or error and never take the gateway down with them.

An imp opts into a listener in its spec:

```toml
# imps/<name>/imp.toml
[channels]
discord = "discord"      # the vault credential its bot uses
```

Config validation fails if two imps claim the same credential (one bot
serving two imps would double-file every message). `--no-listen` runs gateway + dispatch only — the
sanctioned way to boot-test without double-connecting a live bot. `--once`
fires due triggers, drains due tasks, and exits.

## Old → new

Every pre-v2 command prints a pointer to its new home (exit 2) rather than
half-working — argument shapes changed (imps became positional, daemons
merged), so silent translation could misparse.

| old | new |
|---|---|
| `serve` / `supervise` / `listen --imp W` | `server start` |
| `deploy` | `server validate` (config loads live — no deploy step) |
| `gates …` | `server gates …` |
| `channel …` (incl. `memory-*` subcommands) | `server channel …` / `channel set` |
| `connect <p>` / `vault-sync` | `server vault connect <p>` / `server vault sync` |
| `create <n>` | `imp init <n>` |
| `queue add --imp W "p"` | `imp task add W "p"` |
| `relay --imp W "m"` | `imp task relay W "m"` |
| `memory <sub> --imp W` (and `notes`) | `imp memory <sub> W` |
| `memory explain <run>` | `server runs recall <run>` |
| `knowledge W` | `imp knowledge W` |
| `box [--imp W] "p"` / `agent run [-w W] "p"` | `imp run W "p"` (imp now required) |
| `session --imp W` / `agent chat W` | `imp chat W` |
| `runs …` / `agent ls\|show\|context\|recall` | `server runs ls\|show\|context\|recall` |

New in v2 (no old equivalent): `server status`, `server vault ls`,
`imp ls`, `imp show`, `imp trust`.

## The layout underneath

The deployment follows the XDG base dirs (config / data / state; `IMPYARD_ROOT`
for a self-contained root) — see `docs/layout.md` for the full tree and the
migration steps. Config loads live through `src/config.rs`: no deploy step,
mtime-cached, fail-closed on errors. The box mounts no impyard directories at
all — only the engine checkout (`[engine] dir` in org.toml), its own run dir,
channel history, and the CA cert. `imp task add`/`relay` validate the
imp exists before filing, and run ids carry a random suffix so concurrent
dispatch cannot collide on a run directory.
