# Credential injection (spec)

**Status: spec.** The increment after the judge. Fulfills handoff D8 and
build-plan increment 3: **the box stops holding the model credential; the
gateway injects it in transit.** Until now the box carried a throwaway copy
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

## What grows from here

- **OAuth refresh** (the deferred half): the gateway checks `expires`, and
  on expiry calls the provider's token endpoint with the refresh token,
  updates the vault, injects the fresh access token — serving the credential
  "only while the ledger is positive" becomes a gateway concern, never the
  box's. This is also where the **hard budget stop** (D8) attaches: an empty
  ledger ⇒ the gateway declines to inject ⇒ the model call fails at the
  door, mid-run if need be.
- **Real vault** (secrets manager / encrypted-at-rest) replacing the plain
  JSON files; `vault-sync` becomes the owner's credential-loading path.
