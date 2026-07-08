# Credential injection (spec)

**Status: implemented and verified live, 2026-07-08.** All four acceptance
tests below pass. Findings:

- **pi decodes the access token as a JWT** to extract the account id — a
  plain-string sentinel fails with `Failed to extract accountId from token`
  and pi never even makes a network call. The sentinel for a JWT-shaped
  token must be a *well-formed* JWT (far-future `exp`, a sentinel
  `chatgpt_account_id` claim). With that, pi sends it and the gateway swaps
  it. `src/box.ts:sentinelJwt`.
- Verified end to end: the box's `auth.json` holds only sentinels, the real
  access token appears nowhere under the run dir, yet the model calls
  succeed and the task completes — because the gateway injected
  `authorization` + `chatgpt-account-id` in transit. Removing the vault
  entry makes the same call **deny** at the gateway (fail closed); the box
  can't see the vault.
- OAuth **refresh** is now implemented (the gateway owns it — see below).

**Status originally: spec.** The increment after the judge. Fulfills handoff
D8 and build-plan increment 3: **the box stops holding the model credential;
the gateway injects it in transit.** Until now the box carried a throwaway copy
of the real model auth in `.pihome`. After this, the box holds only a
**sentinel** (a deliberately-useless placeholder), and the gateway swaps it
for the real token on the way to the model host. The real key never enters
the box.

## Why the box can hold a sentinel

The box's egress already terminates at the gateway (`docs/judge-spec.md`), so
the gateway rewrites every forwarded request. That means the box's client
(pi) does not need a working credential — it only needs a *structurally
valid* one so it will construct and send the request. The gateway supplies
the truth in transit.

## What this host actually has (verified 2026-07-08)

Both pi providers are **OAuth**, not static API keys:
`{ type, access, refresh, expires, accountId? }`. The default provider is
`openai-codex` → `chatgpt.com`; its access token is valid ~7 days. pi
authenticates with two headers:

```
authorization: Bearer <access>        <- the secret
chatgpt-account-id: <accountId>       <- account identifier (injected too, so the box holds neither)
```

**Scope decision:** this increment injects the current token. Automatic
**OAuth refresh is deferred** to a follow-up (pi's refresh flow is minified
and version-specific; reimplementing it now would be fragile). Consequence,
stated honestly: past the token's validity window the box stops working
until the vault is re-synced (`vault-sync`). The security property — box
never holds the key — holds regardless.

## Design

**Vault (gateway-owned, host-side).** `~/.roster/vault/<name>.json`, outside
the repo and outside the box mount (like the CA). Holds the real credential.
Seeded by a dev verb `vault-sync`, which copies the current pi auth entries
in. "The gateway holds all credentials" becomes literal.

**Injection binds to a rule** (per D15/D16 — meaning attaches to rule
names). A rule gains an optional `inject`:

```json
{ "name": "model-api",
  "match": { "host": ["chatgpt.com", "api.anthropic.com"], "port": 443 },
  "verdict": "allow",
  "inject": { "credential": "openai-codex" } }
```

When that rule fires with verdict `allow`, the gateway looks up the named
credential and rewrites the outgoing headers before forwarding. For an OAuth
credential: set `authorization: Bearer <access>` and, if present,
`chatgpt-account-id: <accountId>`. The set of injected header names is
recorded in the decision (values never — those are the secret).

**Fail closed:** a rule that says `inject` but whose credential is missing
from the vault ⇒ **deny**. Never forward a request that was supposed to be
authenticated but isn't, and never forward the box's sentinel to the real
host.

**The box's sentinel.** The runner stops copying the real auth. It reads the
real entry's *shape*, replaces `access`/`refresh` with obvious sentinel
strings, replaces `accountId` with a placeholder, and pushes `expires` far
into the future (so pi never attempts its own refresh). pi sends the
sentinel; the gateway swaps it. Real values never touch the box.

## Pieces to build

| # | File | ~Size | What |
|---|---|---|---|
| 1 | `src/vault.ts` | ~40 | `getCredential(name)`, `syncFromPiAuth()`. Store at `~/.roster/vault/`. |
| 2 | `src/schema.ts` | +3 | `Rule.inject?: { credential: string }`; `Decision.injected?: string[]`. |
| 3 | `src/gateway.ts` | +~25 | On allow, if the rule injects: render headers from the vault credential, overwrite outgoing headers, record injected names. Missing credential ⇒ deny. |
| 4 | `src/box.ts` | ~edit | Write a **sentinel** auth.json (real shape, secrets nulled to sentinels, expires far future) instead of copying the real one. |
| 5 | `src/cli.ts` | +few | Dev verb `vault-sync`. |
| 6 | `policies/gateway.json` | +1 | Add `inject` to `model-api`. |

Zero new deps. Vault lives outside the box mount → invariant 2 (no secrets
in the box) now holds for the model key too, not just search keys.

## Acceptance — verified live

1. **The box holds no real key.** After a run,
   `runs/<id>/.pihome/agent/auth.json` contains only sentinel strings; the
   real `access` token appears nowhere under the box mount, and `docker
   inspect` shows it in no env var.
2. **Injection makes the call succeed.** The box's sentinel bearer would
   401 on its own; the run nonetheless completes (writes its file) — proof
   the gateway swapped in the real token. The decision record shows
   `injected: ["authorization","chatgpt-account-id"]`.
3. **Fail closed.** With the vault entry removed, the same run is **denied**
   at the gateway (never forwarded with the sentinel), and the box run fails
   cleanly.
4. **Vault is off the box.** `~/.roster/vault/` is not under any box mount;
   from inside the box it does not exist.

## Build order

1. `vault.ts` + `vault-sync` verb; sync and confirm the vault file exists,
   real token off the repo. Commit.
2. `schema.ts` + `gateway.ts` injection + `policies` inject directive; test
   injection host-side (curl with a sentinel bearer through the proxy →
   gateway swaps → real host authenticates). Commit.
3. `box.ts` sentinel; run the box → **acceptance 1, 2**. Commit.
4. **Acceptance 3, 4**; docs + decision-log note. Commit.

## OAuth refresh (implemented 2026-07-08)

The gateway owns refresh — **no dependency on pi's code in the credential
path.** Before injecting, it calls `getFreshCredential(name)`, which refreshes
if the token has expired (within a 60 s skew) and returns a usable credential.

- **`src/providers.ts`** — a per-provider table of *public* constants (token
  endpoint, client id, body encoding, expiry skew), lifted once from each
  provider's own OAuth client, plus a standard `refresh_token` grant. Adding
  a provider is a table row + those few facts.
- **`src/vault.ts:getFreshCredential`** — expiry check → refresh → **merge**
  (so `accountId`/`type` survive; refresh returns only access/refresh/expires)
  → **atomic write** (temp + rename; a half-written rotation would lock us
  out) → audit line to `runs/credentials.jsonl` (never token values). A
  **single-flight lock** gives one refresh lane per credential, so concurrent
  expired requests don't double-refresh and fail on the rotated token.
- **Fail closed**: a refresh error ⇒ the gateway denies the request; it never
  injects a stale token.

**Verified**: 7 unit tests (expiry decision, response mapping, provider skew,
malformed-response fail-closed) + an integration test driving the full success
path against a local mock endpoint (expired → refresh → rotation captured →
merge → persisted). Live: the real Anthropic endpoint returned a structured
`invalid_grant` (its refresh token is dead — proves endpoint/client_id/
encoding + the fail-closed path); a box run with a valid openai-codex token
confirmed the valid path skips refresh. **Not** exercised: a real
openai-codex rotation — it would consume/rotate the in-use token and desync
the host `pi` login, so it's left to happen naturally at expiry.

**Operational contract — refresh tokens rotate.** Once the gateway refreshes,
the provider invalidates the old refresh token, so the host's
`~/.pi/agent/auth.json` goes stale. The **vault becomes the sole source of
truth**; `vault-sync` is a one-time bootstrap, *not* something to re-run
(it would import a now-dead refresh token). If the vault chain ever breaks,
re-login host-side via pi, then `vault-sync` once.

## Credential creation — `connect` (implemented 2026-07-09)

The gateway now creates credentials itself, not just refreshes them — the
OneCLI-style "connect a service" step, done our own way (no OneCLI code).
`node src/cli.ts connect <provider>` runs the provider's login flow and writes
the vault; refresh then keeps it alive. `vault-sync` remains as the shortcut
for "I already logged into pi."

- **A generalized provider registry, `providers.json`** — the single source of
  truth, read by *both* the CLI (`connect`) and the gateway (refresh + inject),
  so nothing is inert config. Each provider declares its `auth` kind, refresh
  constants, an `inject` spec (header templates like `Bearer {access}` filled
  from the credential — generalizes OAuth *and* api-key), and a `login` block.
- **Flows** (`src/connect.ts`, our own implementation): `device_code`
  (openai-codex — device usercode → poll → exchange → decode the JWT for the
  account id), `pkce` (anthropic — PKCE authorize URL + local callback server,
  manual-paste fallback), and `api_key` (prompt + store). Adding a provider is
  a registry entry, not code.
- The Rust gateway's refresh (`providers.rs`) and injection
  (`vault.rs:render_injection` via a `{field}` template substitution) now read
  the same registry, so `providers.json` drives the whole credential path.

**Verified**: both login flows initiate correctly against the real endpoints
(a live device code; a well-formed PKCE authorize URL); injection through the
registry still works end to end (box run). **Not** exercised: completing a real
login (needs interactive browser consent) or the api-key path end to end (no
api-key provider in the registry yet — the code path and inject template
support it).

## What still grows from here

- **Hard budget stop** (D8) attaches at this same pre-inject checkpoint: an
  empty ledger ⇒ the gateway declines to refresh/inject ⇒ the model call
  fails at the door, mid-run if need be.
- **Real vault** (secrets manager / encrypted-at-rest, or an OS keychain)
  replacing the plain-JSON files — now more pressing, since after the first
  refresh the vault is the *only* live copy of the credential.
- **Own the OAuth refresh-endpoint constants per provider** as the registry
  grows (each new provider needs its login/refresh endpoints; lift them from
  the provider's own client, or mine OneCLI's Apache-2.0 app defs as reference).
