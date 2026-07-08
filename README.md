# Roster

**Rented intelligence, owned governance.**

Roster is a platform where you describe a "digital worker" in a folder of
config files, deploy it in minutes, and it does proactive, ongoing agentic
work (research, monitoring, curation, correspondence) — while every action
passes through governance machinery the worker cannot touch: a default-deny
policy judge, budget ledgers, per-action-class trust that is earned, human
approval gates on everything irreversible, and a permanent audit record.

The full design, decision log, and build plan live in
[docs/roster-handoff.md](docs/roster-handoff.md).

## Status

Early scaffolding. Built from scratch in small, understandable increments;
each increment runs live and is tested before the next one starts.

## Toolchain

Two languages, split by trust (see D17 in the handoff):

- **The gateway is Rust** (`gateway/`) — the trusted core that terminates
  TLS, parses hostile request/response bodies, judges, and injects/refreshes
  credentials. `cargo build`, `cargo test`.
- **Orchestration is TypeScript** (`src/`) — the box runner, docker
  lockdown, and CLI. Node 24+, native type-stripping, no build step.
  (Caveat: `node --check` can't syntax-check `.ts` — run the file instead.)
- **Near-zero dependencies by policy** on both sides.
- `npm test` runs the gateway's Rust tests.

## Layout

```
gateway/          the trusted core (Rust): TLS termination, judge, vault, refresh
src/              orchestration (TypeScript): box runner, lockdown, CLI, deploy
box/              Dockerfile for the locked-down container image (roster-box)
org.toml          OWNER-ONLY: shared grants + fleet-aggregate caps + metering
workers/<name>/   OWNER-ONLY worker specs (worker.toml) overlaying org.toml
docs/             design docs, the implementation handoff, per-increment specs
runs/             per-run outputs, logs, and runs/compiled/ (all gitignored)
```

## Run

```
node src/cli.ts                 # help
npm test                        # the gateway's Rust unit tests
```

Config is authored as TOML specs and compiled by `deploy`. The box is one pi
session in a locked-down container, governed by the gateway (see
docs/box-spec.md for the cage, docs/judge-spec.md for the judge):

```
docker build -t roster-box box/                  # once
node src/cli.ts create yuko                       # scaffold workers/yuko/worker.toml
node src/cli.ts deploy                            # compile specs → runs/compiled/{policy,budget}.json
(cd gateway && ROSTER_ROOT=.. cargo run) &        # the box's only door out
node src/cli.ts box --worker yuko "write pong to answer.txt"
```

The gateway terminates TLS (with a host-minted CA at `~/.roster/ca/`, whose
private key never enters the box) so it sees the full request — method,
path, headers, body, and any MCP tool call — and judges it against the
compiled policy. Rules and budget limits carry a **scope** (`org` =
fleet-wide, `org/<name>` = one worker), applied to the call's subject by
ancestor match. Outputs land in `runs/<run-id>/workspace/`; every decision is
a JSON line in `runs/decisions.jsonl` with sensitive header values redacted.

The box holds **no real credential** — only a sentinel. The gateway keeps
the real model key in a vault at `~/.roster/vault/` (off the box mount) and
injects it in transit when a policy rule says so:

```
node src/cli.ts vault-sync           # load host pi credentials into the vault
```

So the model key is never inside the container; a rule with `"inject"`
swaps the box's sentinel for the real token on the way to the model host,
and a missing credential fails closed (deny). The gateway also **refreshes**
expired OAuth tokens itself (owning the provider constants in
`gateway/src/providers.rs` — no dependency on the engine's code), so
injected credentials stay live; every refresh is logged to
`runs/credentials.jsonl`. See docs/injection-spec.md.
