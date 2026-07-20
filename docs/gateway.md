# The gateway

The gateway is the one door. Every byte a box sends out passes through it,
and it decides — allowed, or not — using rules you wrote. It also swaps the
box's fake credentials for real ones on the way out, counts what everything
costs, and writes down every decision it makes, forever.

It listens on `:7300` (set with `server start --addr`; by default it binds
loopback plus the docker bridge, not every interface) as part of the one
daemon, and answers `/healthz` for liveness — the reply carries the
deployment's config root, so probes and boxes can tell this deployment's
daemon from another one squatting on the same port. The daemon records its
binding in `state/gateway.json` for the CLI and boxes to follow.

## Seeing inside requests

Host-and-port visibility isn't enough to govern anything real, so the
gateway terminates TLS. It mints a certificate authority once, on the host
(`data/ca/` — the private key never enters any box), and the box is built to
trust it: every ecosystem's CA variables point at the mounted certificate or
a combined bundle. When a box opens `CONNECT github.com:443`, the gateway
completes the handshake itself with a leaf certificate minted for that host,
reads the full decrypted request — method, path, headers, body — judges it,
and only on `allow` opens its own verified TLS connection to the real host
and streams the response back.

WebSocket upgrades are judged on their handshake, then piped. Requests to
the internal action host (`actions.roster.internal`) are served by the
trusted side directly and never forwarded anywhere — that's how action
proposals travel (see [actions-and-trust.md](actions-and-trust.md)).

## Rules

Grants live in `org.toml` (shared) and `workers/<name>/worker.toml` (one worker),
as an ordered list. Evaluation is **first match wins; no match is a deny.**

```toml
[[grant]]
name    = "model-api"
match   = { host = ["chatgpt.com", "api.anthropic.com"], port = 443 }
verdict = "allow"
inject  = { credential = "openai-codex" }

[[grant]]
name    = "web-fetch"
match   = { host = "*", method = "GET" }
verdict = "allow"
```

Every `match` field is optional; an omitted field matches anything; all
present fields must hold:

| Field | Matches on | Form |
|---|---|---|
| `protocol` | `http` / `https` | string or array |
| `host` | request host | exact, `*.suffix.com`, or `*`; string or array |
| `port` | port | number or array |
| `method` | HTTP method | case-insensitive; `*` = any; string or array |
| `pathPrefix` | URL path | prefix match |
| `headerContains` | a header's value | `{ "header-name" = "substring" }` (`""` = presence) |
| `maxBodySize` | payload bytes | matches only if body ≤ N |
| `mcp.method` | JSON-RPC method | string or array |
| `mcp.tool` | `tools/call` tool name | glob (`get_*`); string or array |

Three verdicts:

- **`allow`** — forward, after injection and budget checks.
- **`deny`** — refuse. Explicit deny rules can carve a hole in a broader
  allow, but safety never depends on them; the floor is default-deny.
- **`tunnel`** — don't terminate TLS for this host; raw-pipe it. The escape
  hatch for cert-pinning clients, decided at CONNECT time. A tunneled host
  is judged on host and port only — grant it knowingly.

Rules written in a worker's own file apply only to that worker; org rules apply
to everyone. (The loader tags scopes automatically — you never write them.)

### MCP, governed at tool granularity

When a request body is JSON-RPC, the gateway lifts MCP's own terms into the
judged request: `mcp.method` (`tools/call`, `resources/read`, …) and, for
tool calls, `mcp.tool`. So one rule can allow `list_*`/`get_*`/`search_*`
tools on an MCP server while `create_pull_request` from the same server
falls through to the default deny.

### Legible refusals

A denial is a verdict, never a mystery hang:

- Policy deny → **403** with `x-roster-verdict: deny`, `x-roster-rule`
  naming the rule that decided (when one matched), and a JSON body whose
  `hint` says plainly: this is policy, not an outage.
- Over budget → **402** with `x-roster-verdict: budget`, a `Retry-After`
  for when the window resets, and the same style of hint.

## Credential injection

Real credentials live in the vault (`data/vault/`, one JSON file per name,
mode 0600), seeded by `roster connection add`.
The box carries sentinels — structurally valid placeholders, shaped like the
real thing so clients will send them (even a well-formed fake JWT where a
JWT is expected).

When an allow rule carries `inject`, the gateway drops the sentinel headers
and writes the real ones — rendered from the provider's header templates
(`authorization: Bearer {access}`, `private-token: {key}`, and so on) —
before forwarding. The decision record notes *which* header names were
injected; values are never logged.

A template entry may name the `hosts` it applies to, so one credential can
wear a different scheme per destination (GitHub's API takes `token {key}`;
its git smart-HTTP endpoints want Basic). For one header name the last
matching entry wins, and `{b64:…}` base64-encodes its substituted body —
how a template builds Basic auth: `Basic {b64:x-access-token:{key}}`.

Fail closed, always: if a rule says inject but the credential is missing or
can't be refreshed, the request is **denied** — the gateway never forwards a
sentinel to a real host.

**OAuth refresh is gateway-owned.** Before injecting, it checks expiry
(60-second skew) and refreshes through the provider registry: single-flight
per credential so concurrent requests don't race a rotating token, merged
and written atomically so a crash can't half-rotate the vault, audited to
`audit/credentials.jsonl` (events only, never values). If a refresh chain
ever breaks, `roster connection add <provider>` re-authenticates.

## Identity: who is asking

The trusted runner mints a random single-use token per run, registers it
host-side as `{subject, run_id}`, and hands it to the box as its proxy
credential. The gateway resolves token → subject (`org/<worker>`) on every
request. A box holds only its own token, so attribution is un-spoofable; a
connection with no token (host-side tooling) is attributed to `org`.

Subjects are paths, and scopes match by ancestry: a rule or budget at `org`
governs every worker; one at `org/dobby` governs only dobby. Spend rolls up the
same way — a call by dobby debits dobby's counters *and* the org's.

## Budgets

Budgets are ledgers, not vibes. Configured in `org.toml`:

```toml
[budget]
currencies = ["usd", "model_calls", "searches"]
vars       = { price = { model_call = 0.05, search = 0.005 } }

[[budget.meter]]
match = 'decision.rule == "model-api"'
spend = { model_calls = "1", usd = "vars.price.model_call" }

[[budget.limit]]
currency = "usd"
window   = "day"
max      = 20.0
```

- **Currencies** are names you define; `usd` is just one of them.
- **Meters** are CEL expressions over `request`, `decision`, `subject`, and
  your `vars`: a match predicate picks calls, and each spend expression maps
  them to amounts.
- **Limits** cap a currency's draw over a `minute`/`hour`/`day`/`month`
  window, at org scope or per-worker (`[[budget.limit]]` in a worker.toml).

Enforcement happens per allowed request, before forwarding: compute spend,
check every applicable scope's limit, refuse with 402 if any is exhausted,
otherwise forward, then debit. The call that crosses the line completes; the
*next* one is refused. Every debit appends to `audit/usage.jsonl`, and the
in-memory counters rebuild from it on restart — rebooting the daemon never
resets a budget.

Two enforcement layers work together (see [work.md](work.md)): the
**soft stop** at dispatch skips *proactive* tasks for an over-budget worker
(your work always runs), and the **hard stop** here at the gateway refuses
governed calls mid-run — including the model call, since serving the model
credential is itself a governed, metered request.

Honest limits: metering counts what the gateway observes directly — request
counts, sizes, per-call prices. Reading token usage out of provider
responses isn't implemented yet, so price model calls per-call rather than
per-token. Windows are fixed-length and aligned to the epoch (a `month` is
30 days), not calendar boundaries.

## The decision record

Every answer the gateway gives — allow, deny, tunnel, budget-refuse — is one
line in `audit/decisions.jsonl`: verdict, the rule that decided, the subject
and run, the full request facts (method, host, port, path, query, headers,
body size, lifted MCP fields), computed spend, and injected header names.
Sensitive header *values* (`authorization`, `cookie`, `x-api-key`, …) are
redacted; bodies are never stored, only their size. Action dispositions
append to the same log. It is append-only and permanent — the "what happened"
of the whole deployment.
