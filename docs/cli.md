# CLI reference

The `impyard` binary is the whole product: the daemon, the approval desk, and
every admin operation are subcommands of one executable. The grammar mirrors
the product thesis — *rented intelligence, owned governance*:

- **`impyard server …`** — the owned machinery: the daemon, config validation,
  the approval desk, channel edges, and the run log (every session, whoever
  ran it).
- **`impyard connection …`** — service capabilities and the imps allowed to
  use them.
- **`impyard credential …`** — provider authentication held on the host.
- **`impyard imp …`** — the governed identities: lifecycle, trust, memory,
  knowledge, each imp's durable task queue — and running sessions as one,
  directly or interactively.

Conventions, everywhere:

- `impyard help <cmd…>` and `-h/--help` work on every node.
- `-V/--version` prints the release plus the git build hash, so a stale
  running daemon can be told apart from the binary you just built.
- IDs (tasks, gates, runs) accept any unique prefix.
- `ls`/`show`/`status` commands take `--json` for scripting.
- Unknown `--flags` are always errors, never prompt text.

## The tree

```
init              create the config/data/state roots (XDG; idempotent)

server start      [--cap N] [--once] [--no-listen] [--addr HOST:PORT]
server status     [--json]
server validate   parse + check all config, print every error
server gates      ls [--json] | show <id> | approve <id> [note] | deny <id> [note]
server channel    ls [--json] | show <id> | trust <id> | untrust <id>
                  | set <id> <key> <value>
                    keys: mode, memory, memory-inferred, memory-kinds,
                          memory-retention, memory-notes, memory-chars
server runs       ls [--imp W] [--limit N] [--json]
                  | show <run> | context <run> [--all] | recall <run>

connection catalog
connection add    [<service>] [--imp W].. [--org] [--name NAME]
                  [--host H].. [--header TEMPLATE] [--env VAR] [--method M]..
connection ls     [--json]

credential add    <provider>
credential ls     [--json]

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

## `impyard init`

Creates the three deployment roots (config, data, state — see
[layout.md](layout.md)). Idempotent: it fills in anything missing and never
overwrites what exists.

## `impyard server`

**`server start`** (alias: `server run`) runs the one daemon in the
foreground: the gateway accept loop, the task-dispatch loop, and one channel
listener per imp with a `[channels]` binding, all supervised siblings in a
single process. One thing to start, one thing to restart after a rebuild.
Listeners restart with backoff on disconnect and never take the gateway down
with them.

- `--cap N` — max concurrent boxes (default 3).
- `--addr HOST:PORT` — gateway listen address (default `0.0.0.0:7300`).
- `--no-listen` — gateway + dispatch only, no channel listeners. The
  sanctioned way to boot-test without double-connecting a live bot.
- `--once` — fire due triggers, drain due tasks, and exit. Useful for cron
  driving and tests.

**`server validate`** runs the same config loader the daemon uses and prints
every error. Config is read live from disk — there is no deploy step, so
validate is how you check an edit before the next read picks it up.

**`server status`** reports daemon health: components, queue depth, pending
gates, and the compiled config.

**`server gates`** is the approval desk. `ls` shows what's waiting; `show`
prints the exact action that would execute (identity and code gates render a
diff); `approve` executes it idempotently; `deny` records the refusal. Both
accept an optional note. See [actions-and-trust.md](actions-and-trust.md).

**`server channel`** manages chat-channel designations: `trust`/`untrust`
set whether a channel's participants may administer the imp and whether
replies send without a gate; `set` tunes response mode and the channel's
memory policy. See [channels.md](channels.md) and [memory.md](memory.md).

**`server runs`** is the run log. `ls` lists past sessions across all imps;
`show` prints one session's transcript, journal, knowledge commits, and
files; `context` prints the exact compiled prompts the session saw (`--all`
for every turn of a warm session); `recall` prints the memory-recall trace.
"What did the imp see?" is always answerable — see [context.md](context.md).

## `impyard connection`

A connection is one intent — "this imp may act on that service" — expressed
as a single object: login flow, credential, egress grant with injection, and
the env var the box sees. `catalog` lists the built-in presets; `add` runs
the wizard (login → vault → scaffold → validate); `ls` shows the inventory
with scope, hosts, and active/disabled state. Any token-authenticated API
can be connected by naming its host with `--host`. See
[connections.md](connections.md).

## `impyard credential`

Host-held provider authentication. `credential add <provider>` runs the
provider's login flow (device code, PKCE, or API-key prompt) and stores the
result in the vault; run it again to rotate. `credential ls` shows names and
types — never values. Channel credentials (Discord, Slack) are added here
and then bound in an imp's `[channels]` table; they never enter a box.

## `impyard imp`

**`imp init <name>`** scaffolds an imp: its spec (`imp.toml`), its identity
file, and its knowledge repository.

**`imp ls` / `imp show`** list imps and inspect one — spec, budgets and
current spend, queue, gates, memory, knowledge.

**`imp trust <name>`** shows per-action trust: what's granted, what's
earned, and the promotion rules in effect.

**`imp run <name> "<prompt>"`** runs one governed session now, bypassing the
queue. `--ceiling M` caps wall-clock minutes (default 30).

**`imp chat <name>`** opens an interactive warm session fed from stdin, one
message per turn. `--idle SECS` ends it after that much quiet (default 20).

**`imp task`** manages the imp's durable queue:

- `add` files a task. `--proactive` marks it budget-gated at dispatch
  (admin-filed work always runs); `--reorganize` requests the exclusive
  knowledge-reorganization lease; `--repo P --base R` makes it a code task
  in a git worktree of that repo (`--base` defaults to `main`).
  `--reorganize` and `--repo` are mutually exclusive.
- `relay` files an inbound message as a task with untrusted-content framing;
  `--from` records the sender label.
- `ls`, `show`, `requeue` — inspect and re-run.

See [work.md](work.md) for task states and dispatch.

**`imp memory`** inspects and repairs interaction memory: list by scope
(`imp`, `channel`, `user`), show a note, `correct` it (recorded, never a
silent edit), `rm`/`pin`/`unpin`/`disable`/`enable` it, or `compact` the
log. See [memory.md](memory.md).

**`imp knowledge <name>`** prints the path of the imp's bare knowledge
repository; from there, use ordinary git:

```bash
git -C "$(impyard imp knowledge yuko)" log --oneline
```
