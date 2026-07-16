# Architecture

Roster is a control plane for **workers** — software colleagues that do
ongoing, unattended work. The model doing the thinking is rented; everything
that decides what a worker is *allowed to do* runs on your machine, as plain
files and one Rust binary. This page is the map: the two sides of the trust
boundary, the one daemon, and the path a piece of work takes through the
system.

## The trust boundary is the language boundary

The codebase is split down the middle, and the split is the security model:

- **The host side is Rust** — a single `roster` binary. It holds the
  credentials, terminates TLS, judges every request, meters spend, executes
  approved actions, and writes the audit log. Everything trusted lives here.
- **The box side is TypeScript** — the agent engine (pi) and its tools,
  running inside a locked-down Docker container. It is *assumed to be
  compromised* and given nothing worth taking: no secrets, no network route,
  no writable code.

The two sides meet only at serialized boundaries: the gateway's HTTP proxy,
typed action envelopes, and files mounted into the container. A box cannot
call into the trusted side except through those doors, and every door judges
what comes through. See [security.md](security.md) for what this buys and
[box.md](box.md) for the container itself.

## One daemon

`roster server start` runs everything as supervised siblings in a single
process:

- **The gateway** (default `0.0.0.0:7300`) — the only network exit from any
  box. It terminates TLS with a host-minted CA, judges every request against
  your grants (default-deny), injects real credentials in transit, meters
  spend against budgets, and appends every decision to the audit log. It
  also serves the internal action host where workers propose consequential
  actions. See [gateway.md](gateway.md).
- **The dispatch loop** — watches every worker's durable task queue, fires
  heartbeats and recurring templates, and runs due tasks in boxes, up to a concurrency cap.
  Proactive work is budget-gated at dispatch; work you file always runs. See
  [work.md](work.md).
- **Channel listeners** — one per worker with a `[channels]` binding: a Discord
  gateway client or a Slack Socket Mode client, dialing out (nothing listens
  on the internet). They record history, route messages into warm sessions
  or file tasks, and never act on their own. See [channels.md](channels.md).

Listeners restart with backoff and never take the gateway down with them.
There is no separate deploy step anywhere: config is read live from disk,
validated on every load, and a broken edit fails closed — the gateway
denies, dispatch pauses, and `server start` refuses to boot.

## The stores

Code, config, and state never mix. The checkout contains no configuration;
your deployment lives in three XDG roots (full tree in
[layout.md](layout.md)):

- **Config** (`~/.config/roster`) — hand-edited, never machine-written
  except by explicit approval flows: `org.toml`, per-worker `worker.toml` and
  `identity.md`, `connections/`, `providers.toml`.
- **Data** (`~/.local/share/roster`) — durable, the backup set: the
  credential vault, the CA, each worker's queue/journal/gates/memory/knowledge,
  channel history, and the append-only audit logs.
- **State** (`~/.local/state/roster`) — reconstructible: run directories,
  per-run identity tokens, listener locks, recurrence cursors, task views.

### What is scoped where

Per-worker surfaces are one mind (P1 in [security.md](security.md)); the
narrower scopes are about the room and the person, never partitions of the
mind:

| Surface | Scope |
|---|---|
| Channel history, purpose, trust/mode/memory settings | per channel |
| Memory notes | one store per worker; each note scoped `worker` / `channel` / `user`, recalled by context |
| Knowledge repository | per worker |
| Queue, journal, gates, identity, trust, budgets | per worker |

Channel surfaces describe the room. User-scoped memory follows the person
across rooms and is governed by them ([memory.md](memory.md)). Everything
per-worker is the mind itself: what it knows (knowledge), what it has been
asked to do (queue), what it did (journal). If some context's affairs must
be invisible to another's participants, that is two workers, not a
partition within one.

## The life of a task

1. **It gets filed.** By you (`roster worker task add`), by a schedule
   (the heartbeat or a recurring template in its task partition), by an inbound chat message (relayed as
   content, never as a command), by the worker itself (`file_task`), or as a
   continuation when a gate resolves.
2. **Dispatch picks it up.** If it's proactive and the worker is over budget,
   it waits; your work always runs.
3. **Context is compiled.** One deterministic compiler assembles exactly
   what this run may see — identity, runtime policy, channel purpose,
   memory, briefing, the task — and writes an exact trace first. See
   [context.md](context.md).
4. **A box is provisioned.** A fresh container on the no-route network, with
   a single-use identity token, sentinel credentials, a knowledge checkout
   mounted read-write or read-only depending on the run's provenance, and a
   hard wall-clock ceiling.
5. **The worker works.** Every network request exits through the gateway and is
   judged, metered, and logged. Consequential actions are proposed as
   envelopes: some auto-execute under earned trust, the rest become durable
   gates for a human. See [actions-and-trust.md](actions-and-trust.md).
6. **The run ends.** Knowledge changes are validated and integrated as a git
   commit (or quarantined if the run died); the task moves to `done` or
   `needs-review`; the run record, journal, context traces, and audit lines
   remain — permanently answerable: what ran, what it saw, what it asked
   for, who approved what, and what it cost.

Chat works the same way, with one shortcut: a burst of messages in a channel
reuses one **warm session** — a live box fed turn by turn — instead of a
cold container per message. Governance is unchanged; only the process
lifecycle differs.

## Where things live in the source

```
src/
  gateway/     the one door: TLS termination, judge, ledger, budgets, proxy
  credential/  vault, provider registry, login flows, OAuth refresh
  action/      envelopes, gates, trust, executors, SMTP
  work/        the task management system (TMS) and dispatch loop
  run/         box provisioning, warm sessions, run records
  worker/         identity, context compiler, memory, knowledge, boundary
  channel/     Discord, Slack, listener supervision, relay
  cli/         every subcommand
box/
  Dockerfile   the roster-box image: toolbelt + baked engine
  extensions/  the box-side tools (actions.ts, web.ts)
```
