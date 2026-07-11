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

Built from scratch in small, understandable increments; each increment runs
live and is tested before the next one starts. Working today: the locked-down
box, the governed-egress gateway (TLS termination, default-deny judge,
credential injection + OAuth refresh, budget metering), governed web
search/fetch, and the full supervisor + gate machinery — a task queue, the
`supervise` dispatch loop, an approval desk with an earned trust ladder,
schedule triggers, continuations, and the code-task worktree→gated-PR flow
(see `docs/supervisor-spec.md`). Discord channels now have warm conversation
sessions, per-channel purpose and behavior controls, and scoped worker/channel/
user memory with governed writes, bounded recall, participant correction, and
owner inspection (see `docs/memory-spec.md`). Workers also receive isolated
Git-backed world knowledge and per-run scratch space; valid append-only notes are
committed by the host on clean exit (see `docs/knowledge-repo-spec.md`).

## Toolchain

The language boundary is the trust boundary (see D20 in the handoff):

- **The whole trusted host-side is Rust** — one `roster` binary (crate at the
  repo root) with subcommands: `serve` (the gateway — TLS termination, judge,
  vault, refresh, and the action host), `supervise` (the orchestration loop),
  `queue` / `runs` / `gates` / `relay` (task queue, execution history,
  approval desk, inbound edge),
  `listen` / `channel` / `memory` (Discord and interaction-memory control),
  `knowledge` (world-knowledge history), plus
  `create` / `deploy` / `box` / `connect` / `vault-sync`. `cargo build`, `cargo test`.
- **TypeScript lives only inside the untrusted box** — pi (the engine,
  vendored) and its extensions (`box/extensions/`: web search/fetch, and the
  action tools). They reach the Rust side across the container contract, a
  serialized boundary that's serialized anyway.
- **Near-zero dependencies by policy.** `npm install` provides pi to the box;
  there is no host-side Node code.
- `npm test` runs the gateway's Rust tests (`cargo test`).

## Layout

```
Cargo.toml src/   the trusted host-side control plane (Rust): the `roster` binary
box/              Dockerfile + extensions/ for the locked-down container (roster-box)
providers.json    provider registry (login/refresh/inject), read by CLI + gateway
org.toml          OWNER-ONLY: shared grants, actions, trust, caps + metering
workers/<name>/   OWNER-ONLY worker specs (worker.toml) overlaying org.toml
docs/             design docs, the implementation handoff, per-increment specs
runs/  queue/  gates/  journal/  channels/  memory/  knowledge/  runtime state (all gitignored)
```

The Rust modules under `src/`: the serve path (`proxy`, `tls`, `ca`, `judge`),
credentials (`vault`, `providers`, `registry`), budgets (`budget`, `ledger`,
`scope`), the supervisor/governance layer (`action`, `gate`, `trust`, `queue`,
`trigger`, `journal`, `memory`), the schema, and the subcommands in `src/cmd/`.

## Run

Build the binary once (`cargo build`; `roster` = `target/debug/roster`), and
`npm install` to provide pi to the box. Run from the repo root (config and
`node_modules` resolve relative to it). Config is authored as TOML specs and
compiled by `deploy`:

```
docker build -t roster-box box/          # once
roster create yuko                        # scaffold workers/yuko/worker.toml
roster deploy                             # compile specs → runs/compiled/*.json
roster serve &                            # the box's only door out (gateway)
roster box --worker yuko "write pong to answer.txt"
roster queue ls                           # durable tasks, newest activity first
roster runs ls                            # all executions, including Discord sessions
roster runs show <run-id>                 # metadata, conversation, journal, memory, files
roster knowledge yuko                     # print the worker's bare Git repository path
git -C "$(roster knowledge yuko)" log      # use normal Git commands after discovery
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
roster connect openai-codex   # log in via the provider's own flow, write the vault
roster vault-sync             # or: import an existing pi login
```

`connect` runs the provider's login (device-code, PKCE, or an API key) and
writes the credential to the vault; the set of providers and how to inject
each lives in `providers.json` (shared by `connect` and the gateway). So the
model key is never inside the container; a rule with `"inject"`
swaps the box's sentinel for the real token on the way to the model host,
and a missing credential fails closed (deny). The gateway also **refreshes**
expired OAuth tokens itself (owning the provider constants in
`src/providers.rs` — no dependency on the engine's code), so
injected credentials stay live; every refresh is logged to
`runs/credentials.jsonl`. See docs/injection-spec.md.
