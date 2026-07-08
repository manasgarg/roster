# The judge and the inspecting gateway (spec)

**Status: implemented and verified live, 2026-07-08.** All six acceptance
tests below pass. Findings from building it:

- **pi's model client honors `NODE_EXTRA_CA_CERTS` through full TLS
  interception** — the load-bearing assumption held; the box happy path is
  green with the gateway terminating TLS. The `tunnel` escape hatch was
  built but is not needed for pi.
- A real pi model call is now fully visible to the judge:
  `POST chatgpt.com/backend-api/codex/responses`, ~6.5 KB JSON body,
  `authorization` redacted in the record, allowed by rule `model-api`.
- MCP governs at tool granularity live: same server + method,
  `tools/call get_issue` allowed by `github-mcp-readonly`,
  `tools/call create_pull_request` hits default-deny.
- Method/path discrimination works: with only `POST` to `chatgpt.com`
  allowed, a `GET` is denied and the record names the method that lost.
- The CA private key is absent from the box (lives at `~/.roster/ca/`,
  outside the mount); only `ca.crt` is mounted. A broken policy denies
  everything (fail closed).

**Status originally: spec.** The increment after the box. Turns the gateway
from a hardcoded two-host allowlist into a policy-driven judge that matches
on
**every parameter of a request** — protocol, method, host, port, path,
query, headers, payload — and on **MCP** tool calls carried inside those
requests. Grounded in the box that already works (`docs/box-spec.md`) and in
the standard-egress-proxy pattern (host-minted CA, terminate-inspect-forward)
that NanoClaw/OneCLI and every corporate proxy use.

## Why this shape

The owner set two constraints that fix the design:

1. **No invented action taxonomy.** Don't ship a fixed list like
   `acquire-source`/`spend-external-api` (the reference implementation's
   approach) — it varies too much per deployment. Rules match on the
   **standard HTTP vocabulary** every egress already speaks. The only
   invented token is each rule's `name`, and *that* is where
   deployment-specific meaning lives: budgets, trust, and gates will later
   bind to rule names, not to a universal class list.
2. **Match on all request parameters, and support MCP.** Both demand the
   same thing: the gateway must **see inside** requests. A blind TLS tunnel
   sees only host+port. So the gateway terminates TLS, inspects the full
   request, judges, and re-originates.

## The physical change: the gateway opens the envelope

Today HTTPS traffic tunnels through the gateway encrypted — it sees
`chatgpt.com:443` and nothing more. To judge on path/headers/body the
gateway must be the TLS endpoint:

- **A host-minted CA.** One CA key + cert generated once, stored at
  `~/.roster/ca/` — **outside the repo, never under the box's mount.** The
  CA *private key* never enters the box. Only the public `ca.crt` is mounted
  read-only into the box, and the box is told to trust it
  (`NODE_EXTRA_CA_CERTS`, `CURL_CA_BUNDLE`, `REQUESTS_CA_BUNDLE`).
- **Per-host leaf certs, minted on demand.** When the box opens
  `CONNECT chatgpt.com:443`, the gateway completes the TLS handshake itself,
  presenting a leaf cert for `chatgpt.com` signed by our CA (SNI-driven,
  cached). The box trusts it because it trusts our CA.
- **Terminate → inspect → judge → forward.** The gateway now holds the
  request in plaintext: method, full path, every header, the whole body. It
  builds the judged request, asks the judge, and only on `allow` opens its
  *own* verified TLS connection to the real host and streams the response
  back.

Result: the two-tier "host-only vs full" visibility split is gone. Every
request — including the model calls — is fully visible and fully judged.

## The escape hatch: `tunnel`

Some clients pin certificates and will refuse an intercepted connection. A
rule may set `"verdict": "tunnel"` for such a host: the gateway does **not**
terminate, it raw-pipes (accepting host-only rules for that host,
explicitly and logged). This is also the fallback if TLS interception turns
out to break pi's model client — set the model hosts to `tunnel` and still
inspect everything else. Decided at CONNECT time (host+port only); anything
not matched as `tunnel` gets terminated and fully judged.

## MCP support

MCP over HTTP is JSON-RPC in a POST body — and we now see bodies. When a
request body parses as JSON-RPC, the gateway lifts MCP's **own** standard
terms into the judged request (no invented vocabulary):

- `mcp.method` — e.g. `tools/call`, `resources/read`, `tools/list`
- `mcp.tool` — for `tools/call`, the `params.name`

Rules then match at tool granularity:

```json
{ "name": "github-mcp-readonly",
  "match": { "host": "api.githubcopilot.com",
             "mcp": { "method": "tools/call", "tool": ["list_*", "get_*", "search_*"] } },
  "verdict": "allow" }
```

Read tools pass; a `create_pull_request` from the same server matches no
rule and hits the default deny.

**Local (stdio) MCP servers** run **host-side**, managed outside the box,
reached by the box as HTTP MCP endpoints through the same door. Not a
workaround: stdio servers hold credentials (tokens, DB passwords), and
host-side is where those belong — the box never sees them. (Host-side stdio
management is a later increment; this one governs remote/HTTP MCP, which is
the part that crosses the door.)

## The rule language (all standard fields)

A policy is `{ "rules": [ Rule, ... ] }`, evaluated top to bottom,
**first match wins, no match ⇒ deny**. A rule is
`{ "name": string, "match": Match, "verdict": "allow"|"deny"|"tunnel" }`.
Every `Match` field is optional; an omitted field matches anything; all
present fields must hold (AND).

| Match field | Matches on | Form |
|---|---|---|
| `protocol` | `http` / `https` | string or array |
| `host` | request host | exact, `*.suffix.com`, or `*`; string or array |
| `port` | port | number or array |
| `method` | HTTP method | case-insensitive; string or array |
| `pathPrefix` | URL path | `startsWith` |
| `headerContains` | a header value | `{ "header-name": "substring" }` (`""` = presence) |
| `maxBodySize` | payload bytes | rule matches only if `bodySize ≤ N` |
| `mcp.method` | JSON-RPC method | string or array |
| `mcp.tool` | `tools/call` tool name | glob (`get_*`); string or array |

Explicit `deny` rules are allowed (to carve a hole in a broader allow), but
safety never depends on them — the floor is default-deny.

## The judged request and the decision record

The judge answers in the same vocabulary it was asked in. Every answer is
one line in `runs/decisions.jsonl` (renamed from `gateway.jsonl` — this is
the real decision record now):

```json
{ "decision_id": "<uuid>", "ts": "<iso>", "verdict": "allow", "rule": "model-api",
  "request": { "worker": null, "protocol": "https", "method": "POST",
    "host": "chatgpt.com", "port": 443, "path": "/backend-api/codex/responses",
    "query": "", "headers": { "authorization": "<redacted>", ... },
    "bodySize": 14231, "mcp": null } }
```

Sensitive header **values** (`authorization`, `cookie`, `x-api-key`,
`proxy-authorization`, `set-cookie`) are redacted to `<redacted>` — presence
is recorded, values never (handoff §10 security habits). Bodies are **not**
stored, only `bodySize` (+ lifted MCP fields).

## Pieces to build

| # | File | ~Size | What |
|---|---|---|---|
| 1 | `src/schema.ts` | ~50 | Types: `GovernedRequest`, `Match`, `Rule`, `Policy`, `Verdict`, `Decision`. The narrow waist. |
| 2 | `src/judge.ts` | ~70 | Pure `judge(req, policy) → {verdict, rule}`. First-match-wins, default-deny, wildcard host + glob tool matching. Repo's first unit tests. |
| 3 | `policies/gateway.json` | small | The ordered rule list, owner-editable, under the read-only mount (worker can't edit it). Seeds with the `model-api` rule = today's two hosts. |
| 4 | `src/ca.ts` | ~60 | CA + per-host leaf minting via `openssl`, `SecureContext` cache. Stores at `~/.roster/ca/`. |
| 5 | `src/gateway.ts` | rewrite | Terminate TLS (SNI → minted cert), buffer + inspect body, lift MCP, consult judge, record decision, forward on allow. `tunnel` short-circuit at CONNECT. Loads policy per request (live edits; **fail closed** on unparseable policy). |
| 6 | `src/box.ts` | +few | `ensureCA()`, mount `ca.crt` read-only, set the three CA-trust env vars. |

Zero new npm dependencies (`node:tls`, `node:https`, `node:crypto`,
`openssl` CLI). CA key stays outside the mount → invariant 2 (no secrets in
the box) holds.

## Acceptance — verified live before done

1. **Judge unit tests** pass (`npm test`): host wildcards, method/port/path,
   `maxBodySize`, MCP method+tool globs, first-match-wins, default-deny,
   empty/broken policy ⇒ deny.
2. **Happy path through the terminating gateway**: `box "write pong…"` still
   works — proves pi's model client honors `NODE_EXTRA_CA_CERTS` through TLS
   interception. (If it does not: set model hosts to `tunnel`, re-run, and
   record that pi needs the escape hatch — a real, documented outcome.)
3. **Full-parameter visibility**: the decision record for that run shows
   `method`, full `path`, `headers` (auth redacted), `bodySize` — not just
   host. With a policy allowing only `POST` to `chatgpt.com`, a `GET` from
   the box is denied, and the record shows the method that lost.
4. **MCP lifting**: a JSON-RPC `tools/call` POST to an allowed host is judged
   with `mcp.method: "tools/call"` and the tool name in its decision record;
   a unit test covers the lifting directly.
5. **CA key never in the box**: from inside the box, `~/.roster/ca/ca.key`
   does not exist (it's outside the mount); `ca.crt` is present and readable.
6. **Fail closed on broken policy**: with `policies/gateway.json`
   unparseable, every request denies.

## Build order (each live before the next)

1. `schema.ts` + `judge.ts` + `policies/gateway.json` + unit tests →
   **acceptance 1**. Commit.
2. `ca.ts`; mint a cert, eyeball it with `openssl x509 -text`. Commit.
3. Rewrite `gateway.ts` to terminate + judge + forward; `box.ts` CA wiring;
   re-run happy path → **acceptance 2, 3**. Commit.
4. MCP lifting + live check → **acceptance 4**. Commit.
5. **Acceptance 5, 6**; update README + box-spec + decision log. Commit.

## Decision-log additions (settled here; recorded in the handoff)

- **D15 — no invented action-class taxonomy.** Governed requests are matched
  on the standard HTTP vocabulary + MCP's own terms; deployment-specific
  meaning attaches to owner-named rule `name`s. Replaces the reference's
  fixed action classes. Budgets/trust/gates bind to rule names.
- **D16 — the gateway terminates TLS with a host-minted CA** to see full
  requests; CA key stays off the box; `tunnel` verdict is the escape hatch
  for cert-pinning clients and the interception-breaks-pi fallback.

## What grows from here

Full-request visibility unlocks, in later increments: **credential
injection** (box sends no key; gateway strips/injects auth in transit — the
"model key behind the gateway" increment becomes a rule feature), **byte- and
token-accurate budgets**, and **`/v1/*` gateway endpoints** as just more
governed requests. One vocabulary, all the way down.
