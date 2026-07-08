# The box — pi in a locked-down container (spec)

**Status: implemented and verified live, 2026-07-08.** All seven acceptance
tests below pass (test 7's "docker unable to recreate the network" half was
not exercised — that needs a deliberately broken Docker; the gateway-down
half was). Findings from implementation, where reality amended the spec:

- The host's default pi provider is **openai-codex**, whose model host is
  `chatgpt.com` — the allowlist is `{api.anthropic.com, chatgpt.com}`, the
  hosts for the two credentials present on this machine.
- `settings.json` is **rebuilt, not copied** into the box: only
  `defaultProvider/defaultModel/defaultThinkingLevel` carry over. The
  host's settings had a `packages` list that made pi `npm install` at boot
  inside the box — the gateway denied `registry.npmjs.org` (default-deny
  earning its keep on day one) and pi died. What the box runs is the
  runner's decision, not inherited host state.
- pi's model calls do honor `HTTPS_PROXY`/`NODE_USE_ENV_PROXY=1` — the
  happy path ran on the no-route network with exactly one
  `CONNECT chatgpt.com allow` in the gateway log.

This increment: run one pi session inside a Docker
container that is locked down, from one command. Grounded in two working
implementations — Yuko (`packages/driver/src/{container,lockdown}.ts`,
`broker/src/broker.ts`, `runtime/Dockerfile`) and NanoClaw
(`/home/manas/projects/yuko/nanoclaw/src/{egress-lockdown,container-runner}.ts`,
30k★, evaluated in `vendor-nanoclaw.md` / `spike-nanoclaw-2026-07-06.md`).
The mechanisms below are verified-live in at least one of them, not invented
here; where they disagree, the choice and reason are stated.

## Goal

```
node src/cli.ts box "what is 2+2? write the answer to answer.txt"
```

starts a container, pi runs inside it, the answer file appears on the host,
and while it ran:

- the code pi runs (pi itself, this repo) was mounted **read-only** — the
  agent cannot edit its own rules or tools;
- **no secrets** were inside the box except the one accepted exposure
  (model auth, see below); the repo's `.env` is shadowed so it can't even be
  read off the read-only mount;
- the container had **no network route to the internet** — its only way out
  was a gateway process on the host, which allows model-API hosts and
  refuses everything else;
- a **hard ceiling timeout** killed the container if the session ran too
  long (the design requires this; the reference never implemented it — we
  do);
- everything pi produced (session log, workspace files) landed host-side,
  owned by the host user.

## Non-goals (this increment)

No policy judge, budgets, or trust. No pi extensions, journal, or mailbox.
No task queue or supervisor. No channels. One worker, one session, run by
hand. The gateway here is a **seed**: hardcoded allowlist + a log — the real
judge grows in its place later without moving the door.

## How the lockdown works — two independent layers

**Layer 1: no route.** The container joins a Docker bridge network created
with IP masquerade (NAT) **disabled**. Containers on it can still reach the
host (L2, not NAT), but packets to the internet leave with a private source
address that replies can't route back to — outbound connections never
complete. There is no internet, full stop.

**Layer 2: no keys.** Search/API keys never enter the box: not in its env,
and the repo's `.env` is bind-mounted over with `/dev/null`. A prompt-injected
agent that wants to exfiltrate or call services directly has nothing to do it
with and no route to do it over.

The only door: `HTTP_PROXY`/`HTTPS_PROXY` (plus lowercase forms,
`NODE_USE_ENV_PROXY=1` for Node's fetch, and `NO_PROXY=` emptied) all point
at the host gateway. The gateway tunnels `CONNECT` requests to an allowlist
of model hosts and answers 403 to everything else, logging every decision.

**Fail closed, always.** If the locked network can't be created or the
gateway isn't listening, the runner **refuses to start the box** — it never
falls back to open egress. This is borrowed from NanoClaw
(`EgressLockdownError` in `egress-lockdown.ts`: "throw rather than silently
spawn an agent with open egress"). The Yuko reference gets this wrong — its
`run.ts` logs a warning and runs with open egress when the network can't be
created. Do not inherit that.

## How the two references do it (and what Roster takes from each)

Both implement the same idea — no internet route + proxy env pointed at the
one governed door — with different Docker mechanisms:

- **Yuko**: a bridge network created with
  `-o com.docker.network.bridge.enable_ip_masquerade=false`. NAT is off, so
  internet-bound packets get no reply route; the **host stays reachable**
  (L2), so the gateway is a plain host process reached via
  `host.docker.internal` (`--add-host …:host-gateway`).
- **NanoClaw**: `docker network create --internal` — stricter, no host
  route at all. Because the host is unreachable, its gateway (OneCLI) must
  itself run **as a container** attached to the same internal network with
  `--alias host.docker.internal`. NanoClaw's gateway also does in-transit
  credential injection (HTTPS_PROXY plus a CA certificate and credential
  stubs mounted into the box) — keys never enter the container at all.

**Roster takes:** Yuko's network mechanism (masquerade-off bridge + host
gateway process — no gateway container to build and manage yet; verified
live with pi specifically); NanoClaw's fail-closed spawn behavior, its
non-root/no-added-capabilities stance (the agent can't undo the lockdown
without `NET_ADMIN`), and — later, not this increment — its opt-in
`--cpus`/`--memory` resource caps and its in-transit credential-injection
model, which is exactly where the "model key behind the gateway" increment
(handoff §9, increment 3) is headed.

**Roster rejects** (per the settled adopt-vs-build verdict, handoff D13):
NanoClaw's runtime itself, its two-SQLite-DB host↔container IO contract
(Roster keeps the file-based task/journal/mailbox contract, §7.3), and the
OneCLI dependency. One deliberate difference: NanoClaw has **no hard
per-session ceiling** — a host sweep detects stale sessions instead, to
avoid killing long legitimate work. Roster's design requires a hard ceiling
(§3.3); we keep it, per-run configurable, and can add sweep-style nuance at
the supervisor increment.

**The one accepted exposure:** pi needs model credentials to function at
all. A throwaway copy of `~/.pi/agent/{auth.json,settings.json}` is placed in
the run's private `.pihome/` dir and mounted as the container's `HOME`. The
agent can read its own model key — accepted and documented until the
"model key behind the gateway" increment moves it out (handoff §9,
increment 3). Fallback if no `auth.json` exists: pass `ANTHROPIC_API_KEY`
into the container env — same exposure, same caveat.

## Pieces to build

| # | File | ~Size | What it is |
|---|---|---|---|
| 1 | `package.json` | +2 lines | Pin the engine: `@earendil-works/pi-coding-agent` at exact `0.80.3` (the maintained fork — **not** `@mariozechner/*`; version verified live in the reference). npm with committed lockfile. |
| 2 | `box/Dockerfile` | ~10 lines | Image `roster-box`: `node:24-bookworm-slim` + `bash ca-certificates git ripgrep curl python3 jq`. pi is **not** baked in — it comes from the read-only repo mount, so the box always runs the exact pinned version on disk and can't tamper with it. |
| 3 | `src/lockdown.ts` | ~30 lines | Ensure the NAT-disabled bridge exists: `docker network create -o com.docker.network.bridge.enable_ip_masquerade=false roster-locked`. Idempotent. **Fail closed** (NanoClaw's pattern): if the network can't be ensured or the gateway isn't answering on :7300, throw — the runner never spawns a box with open egress. |
| 4 | `src/gateway.ts` | ~100 lines | The seed gateway, hand-rolled on `node:http`/`node:net`, zero deps. `CONNECT host:443` → tunnel iff host ∈ `{api.anthropic.com}` (hardcoded for now); anything else → 403. Plain HTTP requests → 403. Appends one JSON line per decision (`{ts, method, host, verdict}`) to `runs/gateway.jsonl`. Must bind `0.0.0.0` (or the bridge IP) — `127.0.0.1` is unreachable from the box. Port 7300. |
| 5 | `src/box.ts` | ~90 lines | The runner: prepare `runs/<id>/` dirs and `.pihome`, resolve pi's real JS entrypoint out of `node_modules`, build the `docker run` args, spawn, arm the ceiling timer (`docker kill` on expiry), report how the run ended. |
| 6 | `src/cli.ts` | +few lines | New dev verb `box` (not one of the six lifecycle verbs): `node src/cli.ts box "<prompt>" [--ceiling <minutes>]`. |

## Run layout (all under gitignored `runs/`)

```
runs/gateway.jsonl        gateway decision log (append-only)
runs/<run-id>/
  workspace/              pi's working dir — artifacts land here (rw mount)
  session/                pi --session-dir — *.jsonl, the future usage/accounting source (rw mount)
  .pihome/                throwaway HOME with the copied model auth (rw mount)
  stdout.jsonl            pi's --mode json event stream, captured by the runner
```

## The docker invocation (shape, mirroring the verified reference)

```
docker run --rm --name roster-box-<id>
  --add-host=host.docker.internal:host-gateway
  --network roster-locked
  -u <host-uid>:<host-gid>                        # outputs come out host-owned
  -v <repoRoot>:<repoRoot>:ro                     # code + node_modules, read-only, same path
  -v /dev/null:<repoRoot>/.env:ro                 # shadow the secrets file
  -v <runs/<id>/workspace>:<same>                 # rw overlays over the ro repo
  -v <runs/<id>/session>:<same>
  -v <runs/<id>/.pihome>:<same>
  -e HOME=<.pihome> -e PI_CODING_AGENT_DIR=<.pihome>/agent
  -e HTTP_PROXY=http://host.docker.internal:7300  # + HTTPS_PROXY, lowercase forms
  -e NODE_USE_ENV_PROXY=1 -e NO_PROXY=
  -w <runs/<id>/workspace>
  roster-box
  node <pi-entrypoint> --mode json --no-extensions --session-dir <runs/<id>/session> "<prompt>"
```

Notes: the repo mounts at its **same absolute path** so `node_modules`
resolution works unchanged. pi's entrypoint is resolved from
`node_modules/@earendil-works/pi-coding-agent/package.json`'s `bin` field
(the `.bin` shim is a shell script; resolve the real JS file). No `-e`
extension flags yet — pi runs with built-in tools only until the behavior-kit
increment.

## Container contract (destination vs. this increment)

The handoff's contract (§7.3) is the destination; this increment implements
a reduced form. Deltas, so we know what we're deferring:

| Contract says | This increment | Arrives with |
|---|---|---|
| In: task file + mailbox dir | In: prompt string on argv | task queue increment |
| Out: `journal/events.jsonl` | Out: session `*.jsonl` + workspace files | behavior-kit (extensions) increment |
| Run until `close_task` or ceiling | Run until pi exits or ceiling | `close_task` needs extensions |
| Egress: gateway only | Same — real from day one | — |

## Acceptance — run these live before calling it done

1. **Happy path**: `node src/cli.ts box "write 4 to answer.txt"` →
   `runs/<id>/workspace/answer.txt` exists on the host, host-owned. Proves
   pi's model calls honor `HTTPS_PROXY` through the tunnel (the reference
   flags this as the one thing to verify per setup — it is the load-bearing
   assumption).
2. **No direct egress**: a box prompt asking pi to
   `curl --noproxy '*' https://example.com` fails (no route); plain
   `curl https://example.com` (which honors the proxy) gets the gateway's
   403 and both show up as deny lines in `runs/gateway.jsonl`.
3. **Read-only code**: a box prompt asking pi to append a line to a repo
   file fails with a read-only filesystem error.
4. **No secrets**: put a canary value in `.env`; from inside the box,
   `cat <repoRoot>/.env` is empty. `docker inspect` on the running container
   shows no env var containing the canary.
5. **Ceiling**: `--ceiling 1` (minute) on a prompt that loops → container
   is killed at the ceiling, runner exits reporting `ceiling`, no orphan
   container remains (`docker ps` clean).
6. **Gateway is honest**: every model call in run 1 appears as an allow
   line for `api.anthropic.com`; nothing else was allowed all session.
7. **Fail closed**: with the gateway process not running (and separately,
   with the `roster-locked` network deleted and Docker unable to recreate
   it), `node src/cli.ts box …` refuses to start, says why, and `docker ps`
   shows no container was ever spawned.

## Build order (each step verified before the next)

1. Pin pi, `npm install`, resolve + run its entrypoint on the **host**
   (`--mode json`, no container) — proves the engine runs at all.
2. `box/Dockerfile`, build image; run pi in the container **without**
   lockdown (default bridge, no proxy) — proves mounts, uid, HOME, paths.
3. `src/gateway.ts` alone; from host, `curl -x localhost:7300` an allowed
   and a denied host — proves tunnel + 403 + log.
4. `src/lockdown.ts` (fail-closed) + wire proxy env into the runner;
   re-run the happy path locked down — **acceptance 1, 2, 6, 7**.
5. Ceiling timer, `.env` shadow check — **acceptance 3, 4, 5**. Commit.

## Choices made here (small, reversible — flag disagreement early)

- **npm** (not pnpm) — one less tool; lockfile committed.
- Gateway port **7300**; network name **roster-locked**; image
  **roster-box**; container names **roster-box-<run-id>**.
- Ceiling default **30 minutes**, per-run override via `--ceiling`.
- Allowlist hardcoded to `api.anthropic.com` until the judge exists.
- `box` is a dev verb; it will fold into `deploy`/the supervisor's session
  runner later, and `src/box.ts` becomes that runner's core.

## What grows from each piece

- `src/gateway.ts` → the real gateway: judge consultation, in-transit key
  injection (the NanoClaw/OneCLI model — keys never enter the box, arriving
  with the model-key increment), `/v1/search`·`/v1/fetch` endpoints,
  decision records (its `gateway.jsonl` is the embryo of `decisions.jsonl`).
- `src/box.ts` → the supervisor's session runner (task files, mailbox,
  journal relay).
- `runs/<id>/session/*.jsonl` → token/cost accounting (usage ledger).
- The acceptance list → the standing invariant test suite (handoff §5,
  invariants 2, 3, 4).
