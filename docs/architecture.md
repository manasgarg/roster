# Architecture

Impyard is a control plane for **imps** — software colleagues that do
ongoing, unattended work. The model doing the thinking is rented; everything
that decides what an imp is *allowed to do* runs on your machine, as plain
files and one Rust binary. This page is the map: the two sides of the trust
boundary, the one daemon, and the path a piece of work takes through the
system.

## The trust boundary is the language boundary

The codebase is split down the middle, and the split is the security model:

- **The host side is Rust** — a single `impyard` binary. It holds the
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

`impyard server start` runs everything as supervised siblings in a single
process:

- **The gateway** (default `0.0.0.0:7300`) — the only network exit from any
  box. It terminates TLS with a host-minted CA, judges every request against
  your grants (default-deny), injects real credentials in transit, meters
  spend against budgets, and appends every decision to the audit log. It
  also serves the internal action host where imps propose consequential
  actions. See [gateway.md](gateway.md).
- **The dispatch loop** — watches every imp's durable task queue, fires
  scheduled triggers, and runs due tasks in boxes, up to a concurrency cap.
  Proactive work is budget-gated at dispatch; work you file always runs. See
  [work.md](work.md).
- **Channel listeners** — one per imp with a `[channels]` binding: a Discord
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

- **Config** (`~/.config/impyard`) — hand-edited, never machine-written
  except by explicit approval flows: `org.toml`, per-imp `imp.toml` and
  `identity.md`, `connections/`, `providers.toml`.
- **Data** (`~/.local/share/impyard`) — durable, the backup set: the
  credential vault, the CA, each imp's queue/journal/gates/memory/knowledge,
  channel history, and the append-only audit logs.
- **State** (`~/.local/state/impyard`) — reconstructible: run directories,
  per-run identity tokens, listener locks, trigger cursors.

## The life of a task

1. **It gets filed.** By you (`impyard imp task add`), by a schedule
   (`[[trigger]]` in the imp's spec), by an inbound chat message (relayed as
   content, never as a command), by the imp itself (`file_task`), or as a
   continuation when a gate resolves.
2. **Dispatch picks it up.** If it's proactive and the imp is over budget,
   it waits; your work always runs.
3. **Context is compiled.** One deterministic compiler assembles exactly
   what this run may see — identity, runtime policy, channel purpose,
   memory, briefing, the task — and writes an exact trace first. See
   [context.md](context.md).
4. **A box is provisioned.** A fresh container on the no-route network, with
   a single-use identity token, sentinel credentials, a knowledge checkout
   mounted read-write or read-only depending on the run's provenance, and a
   hard wall-clock ceiling.
5. **The imp works.** Every network request exits through the gateway and is
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
  work/        the durable queue, dispatch loop, triggers
  run/         box provisioning, warm sessions, run records
  imp/         identity, context compiler, memory, knowledge, boundary
  channel/     Discord, Slack, listener supervision, relay
  cli/         every subcommand
box/
  Dockerfile   the impyard-box image: toolbelt + baked engine
  extensions/  the box-side tools (actions.ts, web.ts)
```
