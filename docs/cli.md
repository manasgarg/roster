# CLI reference

The `roster` binary is the whole product: the daemon, the approval desk, and
every admin operation are subcommands of one executable. The grammar mirrors
the product thesis — *rented intelligence, owned governance*:

- **`roster server …`** — the owned machinery: the daemon, config validation,
  the approval desk, channel edges, and the run log (every session, whoever
  ran it).
- **`roster connection …`** — service capabilities and the workers allowed to
  use them.
- **`roster credential …`** — provider authentication held on the host.
- **`roster worker …`** — the governed identities: lifecycle, trust, memory,
  knowledge, each worker's durable task queue — and running sessions as one,
  directly or interactively.

Conventions, everywhere:

- `roster help <cmd…>` and `-h/--help` work on every node.
- `-V/--version` prints the release plus the git build hash, so a stale
  running daemon can be told apart from the binary you just built.
- IDs (tasks, gates, runs) accept any unique prefix.
- `ls`/`show`/`status` commands take `--json` for scripting.
- Unknown `--flags` are always errors, never prompt text.

## The tree

```
init              create the config/data/state roots (XDG; idempotent)
talk              <worker> [--idle SECS]  chat with a worker, right here

server start      [--cap N] [--once] [--no-listen] [--addr HOST:PORT]
server status     [--json]
server validate   parse + check all config, print every error
server gates      ls [--json] | show <id> | approve <id> [note] | deny <id> [note]
server channel    ls [--json] | show <id> | trust <id> | untrust <id>
                  | set <id> <key> <value>
                    keys: mode, memory, memory-inferred, memory-kinds,
                          memory-retention, memory-notes, memory-chars
server runs       ls [--worker W] [--limit N] [--json]
                  | show <run> | context <run> [--all] | recall <run>

connection catalog
connection add    [<service>] [--worker W].. [--org] [--name NAME]
                  [--host H].. [--header TEMPLATE] [--env VAR] [--method M]..
connection ls     [--json]

credential add    <provider>
credential ls     [--json]

worker init       <name>
worker ls         [--json]
worker show       <name> [--json]
worker trust      <name> [--json]
worker run        <name> [--ceiling M] "<prompt>"
worker chat       <name> [--idle SECS]
worker task       add <worker> [--ceiling M] [--proactive|--reorganize]
                      [--repo P --base R] "<prompt>"
                  | relay <worker> [--from WHO] "<message>"
                  | ls [--json] | show <id> | requeue <id>
worker memory     ls <worker> [--scope S] [--scope-id ID] | show <worker> <id>
                  | correct <worker> <id> "<replacement>"
                  | rm|pin|unpin|disable|enable <worker> <id> | compact <worker>
worker knowledge  <name>
```

## `roster init`

Creates the three deployment roots (config, data, state — see
[layout.md](layout.md)). Idempotent: it fills in anything missing and never
overwrites what exists.

## `roster talk`

Your terminal as a chat channel — the Discord/Slack interaction model
without leaving the shell. `roster talk yuko` opens (or resumes) the durable
channel `term-<you>-yuko`: trusted like a DM, history recorded under
`data/channels/`, a purpose the worker can propose, channel and user memory
scopes, and warm-session turns. Replies print straight to your terminal;
Ctrl-D ends the session immediately. `--idle SECS` (default 300) ends it
after that much quiet. See [channels.md](channels.md).

## `roster server`

**`server start`** (alias: `server run`) runs the one daemon in the
foreground: the gateway accept loop, the task-dispatch loop, and one channel
listener per worker with a `[channels]` binding, all supervised siblings in a
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
set whether a channel's participants may administer the worker and whether
replies send without a gate; `set` tunes response mode and the channel's
memory policy. See [channels.md](channels.md) and [memory.md](memory.md).

**`server runs`** is the run log. `ls` lists past sessions across all workers;
`show` prints one session's transcript, journal, knowledge commits, and
files; `context` prints the exact compiled prompts the session saw (`--all`
for every turn of a warm session); `recall` prints the memory-recall trace.
"What did the worker see?" is always answerable — see [context.md](context.md).

## `roster connection`

A connection is one intent — "this worker may act on that service" — expressed
as a single object: login flow, credential, egress grant with injection, and
the env var the box sees. `catalog` lists the built-in presets; `add` runs
the wizard (login → vault → scaffold → validate); `ls` shows the inventory
with scope, hosts, and active/disabled state. Any token-authenticated API
can be connected by naming its host with `--host`. See
[connections.md](connections.md).

## `roster credential`

Host-held provider authentication. `credential add <provider>` runs the
provider's login flow (device code, PKCE, or API-key prompt) and stores the
result in the vault; run it again to rotate. `credential ls` shows names and
types — never values. Channel credentials (Discord, Slack) are added here
and then bound in a worker's `[channels]` table; they never enter a box.

## `roster worker`

**`worker init <name>`** scaffolds a worker: its spec (`worker.toml`), its identity
file, and its knowledge repository.

**`worker ls` / `worker show`** list workers and inspect one — spec, budgets and
current spend, queue, gates, memory, knowledge.

**`worker trust <name>`** shows per-action trust: what's granted, what's
earned, and the promotion rules in effect.

**`worker run <name> "<prompt>"`** runs one governed session now, bypassing the
queue. `--ceiling M` caps wall-clock minutes (default 30).

**`worker chat <name>`** opens a bare interactive warm session fed from
stdin, one message per turn — no channel identity, history, or memory
(useful for testing the session machinery). For actual conversation use
`roster talk`, which is a real channel. `--idle SECS` ends it after that
much quiet (default 20).

**`worker task`** manages the worker's durable queue:

- `add` files a task. `--proactive` marks it budget-gated at dispatch
  (admin-filed work always runs); `--reorganize` requests the exclusive
  knowledge-reorganization lease; `--repo P --base R` makes it a code task
  in a git worktree of that repo (`--base` defaults to `main`).
  `--reorganize` and `--repo` are mutually exclusive.
- `relay` files an inbound message as a task with untrusted-content framing;
  `--from` records the sender label.
- `ls`, `show`, `requeue` — inspect and re-run.

See [work.md](work.md) for task states and dispatch.

**`worker memory`** inspects and repairs interaction memory: list by scope
(`worker`, `channel`, `user`), show a note, `correct` it (recorded, never a
silent edit), `rm`/`pin`/`unpin`/`disable`/`enable` it, or `compact` the
log. See [memory.md](memory.md).

**`worker knowledge <name>`** prints the path of the worker's bare knowledge
repository; from there, use ordinary git:

```bash
git -C "$(roster worker knowledge yuko)" log --oneline
```
