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

- **Node 24+** — TypeScript runs directly via native type-stripping. No
  build step, no `tsc`, no bundler. `node src/cli.ts` just works.
  (Caveat: `node --check` does *not* strip types, so it can't syntax-check
  `.ts` files — run the file or the tests instead.)
- **Near-zero dependencies by policy** — prefer small hand-rolled pieces
  over framework adoption.
- Tests use the built-in `node:test` runner: `npm test`.

## Layout

```
src/       all code (single flat package for now; split only when it hurts)
box/       Dockerfile for the locked-down container image (roster-box)
policies/  the gateway rule list (owner-editable; the worker can't touch it)
docs/      design docs, the implementation handoff, per-increment specs
test/      tests (node:test) — run with `npm test`
runs/      per-run outputs + the decision log (gitignored)
```

## Run

```
node src/cli.ts                 # help
npm test                        # the judge's unit tests
```

The box — one pi session in a locked-down container, governed by the judge
(see docs/box-spec.md for the cage, docs/judge-spec.md for the judge):

```
docker build -t roster-box box/          # once
node src/gateway.ts &                    # the box's only door out
node src/cli.ts box "write the word pong to answer.txt"
```

The gateway terminates TLS (with a host-minted CA at `~/.roster/ca/`, whose
private key never enters the box) so it sees the full request — method,
path, headers, body, and any MCP tool call — and judges it against
`policies/gateway.json`. Outputs land in `runs/<run-id>/workspace/`; every
decision is a JSON line in `runs/decisions.jsonl` with sensitive header
values redacted.
