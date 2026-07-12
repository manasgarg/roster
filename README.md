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
Git-backed world knowledge; valid append-only notes are committed by the host on
clean exit. Disposable downloads and working files use a private, bounded
container `/tmp` that disappears with the container (see
`docs/knowledge-repo-spec.md`).

## Toolchain

The language boundary is the trust boundary (see D20 in the handoff):

- **The whole trusted host-side is Rust** — one `roster` binary (crate at the
  repo root) whose command grammar is the product thesis (see
  `docs/cli.md`): `roster server …` (the owned machinery — the daemon,
  `validate`, the approval desk, channels, the vault), `roster worker …` (the
  governed identities — lifecycle, trust, memory, knowledge, tasks), and
  `roster agent …` (the rented intelligence — run sessions, inspect what they
  saw). `cargo build`, `cargo test`.
- **TypeScript lives only inside the untrusted box** — pi (the engine,
  vendored) and its extensions (`box/extensions/`: web search/fetch, and the
  action tools). They reach the Rust side across the container contract, a
  serialized boundary that's serialized anyway.
- **Near-zero dependencies by policy.** `npm install` provides pi to the box;
  there is no host-side Node code.
- `npm test` runs the gateway's Rust tests (`cargo test`).

## Layout

This repo is only the platform: `src/` (the `roster` binary), `box/`
(Dockerfile + extensions for the locked-down container), `docs/`. **No config
or state lives with the code.** The deployment follows the XDG base dirs —
config in `~/.config/roster` (org.toml, workers/), durable data in
`~/.local/share/roster` (vault, per-worker footprints, audit records), and
prunable state in `~/.local/state/roster` (runs, locks) — with `ROSTER_ROOT`
as a self-contained override. Every path is minted in `src/paths.rs`; the
full tree and migration steps are in `docs/layout.md`. There is no deploy
step: config loads live, validates on every read, and fails closed when
broken.

The Rust modules under `src/`: the serve path (`proxy`, `tls`, `ca`, `judge`),
credentials (`vault`, `providers`, `registry`), budgets (`budget`, `ledger`,
`scope`), the supervisor/governance layer (`action`, `gate`, `trust`, `queue`,
`trigger`, `journal`, `memory`), the schema, and the subcommands in `src/cmd/`.

## Run

Build the binary once (`cargo build`; `roster` = `target/debug/roster`), and
`npm install` to provide pi to the box. Run from the repo root (config and
`node_modules` resolve relative to it). Config is authored as TOML specs and
compiled by `server deploy`:

```
docker build -t roster-box box/            # once
roster init                                # create the config/data/state roots
roster worker init yuko                    # scaffold ~/.config/roster/workers/yuko/
roster server validate                     # parse + check all config (loads live)
roster server run &                        # gateway + task dispatch + channel listeners
roster server status                       # is it up, does config parse, what's pending
roster agent run -w yuko "write pong to answer.txt"
roster worker ls                           # the fleet at a glance
roster worker task ls                      # durable tasks, newest activity first
roster agent ls                            # all executions, including Discord sessions
roster agent show <run-id>                 # metadata, conversation, journal, memory, files
roster worker knowledge yuko               # print the worker's bare Git repository path
git -C "$(roster worker knowledge yuko)" log   # use normal Git commands after discovery
roster worker task add yuko --reorganize "rebuild the topic organization"
```

A worker opts into Discord with a `[channels] discord = "<vault credential>"`
entry in its `worker.toml`; `server run` starts one supervised listener per
entry (`--no-listen` skips them — no more bogus-token tricks to avoid
double-connecting a bot during tests).

The gateway terminates TLS (with a host-minted CA under the data root, whose
private key never enters the box) so it sees the full request — method,
path, headers, body, and any MCP tool call — and judges it against the
compiled policy. Rules and budget limits carry a **scope** (`org` =
fleet-wide, `org/<name>` = one worker), applied to the call's subject by
ancestor match. Outputs land in the state root under `runs/<run-id>/workspace/`; every
decision is a JSON line in the data root's `audit/decisions.jsonl` with
sensitive header values redacted.

The box holds **no real credential** — only a sentinel. The gateway keeps
the real model key in the vault (data root; never mounted into the box) and
injects it in transit when a policy rule says so:

```
roster server vault connect openai-codex   # log in via the provider's own flow
roster server vault sync                   # or: import an existing pi login
roster server vault ls                     # names and types only, never values
```

`vault connect` runs the provider's login (device-code, PKCE, or an API key) and
writes the credential to the vault; provider defaults (login flow, refresh,
injection) ship inside the binary, overridable at `~/.config/roster/providers.toml`. So the
model key is never inside the container; a rule with `"inject"`
swaps the box's sentinel for the real token on the way to the model host,
and a missing credential fails closed (deny). The gateway also **refreshes**
expired OAuth tokens itself (owning the provider constants in
`src/providers.rs` — no dependency on the engine's code), so
injected credentials stay live; every refresh is logged to the audit
record. See docs/injection-spec.md.
