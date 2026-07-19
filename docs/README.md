# Roster documentation

Roster runs **workers** — software colleagues that keep working when you're
not watching — inside governance machinery you own: a default-deny gateway,
injected credentials, budgets as ledgers, human approval gates, earned
trust, and a permanent audit record. *Rent the intelligence; own the
governance.*

## Start here

- **[Getting started](getting-started.md)** — from a fresh machine to a
  working, governed worker.
- **[Architecture](architecture.md)** — the trust boundary, the one daemon,
  and the life of a task.
- **[The security model](security.md)** — the threat model, the mechanisms,
  the invariants, and the honest limits.

## The machinery

- **[The gateway](gateway.md)** — the one door: policy rules, TLS
  interception, credential injection, budgets and metering, the decision
  record.
- **[Actions, gates, and trust](actions-and-trust.md)** — propose/approve/
  execute, the approval desk, and the earned-trust ladder.
- **[Work](work.md)** — the durable task queue, schedules, code tasks, and
  the run log.
- **[Channels](channels.md)** — Discord, Slack, and your terminal:
  authority, response modes, warm sessions, purposes.
- **[The box](box.md)** — the locked-down container: no route, no secrets,
  a real toolbelt.

## What a worker knows

- **[Memory](memory.md)** — worker-owned memory of people and
  conversations, kept in the store.
- **[The store](store.md)** — each worker's durable read-write directory:
  worker-managed layout, snapshots, restore, and the locking discipline.
- **[Repos](repos.md)** — gated host git repositories (the knowledge repo
  among them), and the boundary that keeps person-data out of them.
- **[Compiled context](context.md)** — exactly what each run sees, traced
  byte-for-byte.

## Reference

- **[Configuration](configuration.md)** — every file and key.
- **[CLI](cli.md)** — the full command tree.
- **[Connections](connections.md)** — granting service capabilities.
- **[On-disk layout](layout.md)** — the three roots and the backup story.
