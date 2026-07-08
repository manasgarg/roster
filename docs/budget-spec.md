# Metering & budgets (spec)

**Status: B1–B2 implemented (2026-07-08); B3–B4 pending.** B1 = CEL currency
metering, logged; B2 = the ledger + limits + enforcement (the hard stop) on
count currencies, org-global, with boot-rehydration. Verified live: a
`model_calls` cap denies the over-cap call with `402` (the call that crosses
completes, the next is refused), and the counter survives a gateway restart.
Next: B3 token metering (response tap + adversarial defenses), B4 namespaced
identity.

**Design.** Built on the Rust gateway, with CEL (D18). Realizes the
owner's mental model: a **call log** as the substrate, **namespaced identity**,
**arbitrary currencies** with CEL mappings from request/response to spend, and
**limits on currency drawdown**. Grounded in the research (LiteLLM's budget
model; the adversarial-metering "billing black hole"; Cloudflare/iron-proxy
topology) — see the conversation and `docs/injection-spec.md`.

## The four layers

### 1. The call log (substrate)
One append-only record per governed call (grows from `runs/decisions.jsonl`).
No payload — request/response *facts* plus **derived** signals lifted from
bodies at the gateway (tokens, tool name, model, row counts), and the **spend**
the call drew. Everything else is computed from this.

```
CallRecord { id, ts, subject,
  request  { protocol, method, host, port, path, query, headers:<redacted>, bodyBytes, derived{…} },
  response { status, bodyBytes, durationMs, derived{…} },   // for forwarded calls
  decision { verdict, rule, injected[] },
  spend    { <currency>: <amount>, … } }
```

### 2. Namespaced identity
`subject` is a path (`org/team/worker/session`). Policies and aggregation
attach at any prefix; a call debits every **ancestor** ledger. Org-global
(`subject = "org"`) until worker identity is wired at the gateway.

### 3. Currencies + CEL spend mapping
Currencies are owner-defined names; `usd` is just one, computed from others.
A **meter** matches calls (CEL predicate) and maps them to a spend vector (CEL
expression per currency), over a shared context.

```yaml
currencies: [usd, input_tokens, output_tokens, searches, github_calls]
vars: { price: { in_per_tok: 2.5e-6, out_per_tok: 10e-6, search: 0.005 } }
meters:
  - match: 'decision.rule == "model-api"'
    spend:
      input_tokens:  'response.derived.usage_input'
      output_tokens: 'response.derived.usage_output'
      usd: 'response.derived.usage_input*vars.price.in_per_tok + response.derived.usage_output*vars.price.out_per_tok'
  - match: 'decision.rule == "web-search"'
    spend: { searches: '1', usd: 'vars.price.search' }
```

**CEL context** (one binding surface for judge conditions, extraction, and
meters): `request.*`, `response.*` (post-response only), `decision.*`,
`subject`, `vars.*`.

### 4. Limits on currency drawdown
A limit caps cumulative draw of one currency, at a namespace scope, over a
window. A call computes its spend, checks every ancestor scope's limit, and is
denied if any is over — same pre-forward checkpoint as injection.

```yaml
limits:
  - { scope: "org", currency: usd, window: day, max: 20 }
  - { scope: "org", currency: input_tokens, window: hour, max: 2_000_000 }
```

## Enforcement checkpoint

Per allowed request, right before forward: judge allow → **budget check**
(every applicable limit vs current balance; over any ⇒ deny) → refresh/inject
→ forward → **meter response** → debit ledgers (resource + cost). Semantics:
the call that crosses the line completes; the *next* one is refused (hard stop
"fails mid-run"). This is where the hard-budget stop (D8) lives.

## Metering: the easy case and the hard case

- **Count / bytes currencies are un-falsifiable by construction** — the egress
  lockdown means the box cannot make a call the gateway doesn't see, so request
  count and byte sizes are directly observed. No streaming complications.
- **Token currencies are the hard case** (adversarial metering). Streaming
  usage isn't returned unless asked, and an untrusted box would omit it or
  early-disconnect. Defenses (from the research): the gateway **forces**
  `stream_options.include_usage` by rewriting the request; **pre-counts** input
  tokens at prefill; falls back to a **tokenizer** or **byte-coefficient**
  estimate; and runs a **count/byte cap in parallel** as the black-hole
  backstop. Every token number records its `meterSource`
  (`provider-usage` | `tokenizer` | `byte-estimate`) and an `estimated` flag.

## Ledger & semantics

- **Store**: append-only `runs/usage.jsonl` (`{ts, subject, rule, currency,
  amount}` rolled up from each call's spend) is the source of truth;
  in-memory counters keyed by `(subject, currency, window)` are a cache the
  single-process gateway owns and rehydrates from the log on boot.
- **Windows**: fixed calendar (`minute`/`hour`/`day`/`month`), reset at the
  boundary (LiteLLM's `budget_duration` model). Rolling windows are a later
  refinement.
- **Absent vs uncheckable**: no budget configured ⇒ no cap (the default-deny
  judge is still the security floor; budget is an opt-in ceiling on allowed
  traffic). Configured but uncheckable ⇒ **deny** (fail closed on the
  mechanism, not on a missing one).
- **Owner-only** (D7): caps and prices live in `policies/budget.json`, off the
  box mount; the agent can never raise a cap.

## Build order (each built + verified before the next)

- **B1 — config + CEL spend, logged.** `policies/budget.json` (currencies,
  vars, meters); the gateway evaluates the CEL meters per call and records the
  `spend` vector in the call log. Count currencies only (request-derived). No
  enforcement yet. *Proves config + CEL + spend computation.*
- **B2 — ledger + limits + enforcement.** In-memory counters + `usage.jsonl`;
  check limits before forward (deny over cap), debit after. Org-global.
  *Proves the hard stop on an un-falsifiable count currency.*
- **B3 — token metering.** Response tap + the adversarial defenses; cost/USD
  currencies via the price `vars`; the call log gains response `derived` +
  `meterSource`.
- **B4 — namespaced identity.** Worker identity at the gateway; per-scope
  ledgers and ancestor rollup.

## What we borrow

LiteLLM's budget shape (subject × currency × reset-window, USD via a price
table, per-key + aggregate). The topology (MITM egress + injection) is already
ours. The intersection — token budgets enforced in an untrusted-egress proxy —
is the part no off-the-shelf tool does, so it's the part we build: currencies +
CEL + the adversarial token defenses.
