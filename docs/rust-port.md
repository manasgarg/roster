# Porting the gateway to Rust (plan)

**Superseded by D20 (2026-07-09): the *entire* trusted host-side is now Rust.**
The gateway port (P0–P4) below was the first half; D20 then folded the
orchestration (box runner, lockdown, `create`/`deploy`/`connect`/`vault-sync`)
into the same `impyard` binary and retired all of `src/*.ts`. TypeScript now
lives only inside the box (pi + extensions). One binary, one schema set — the
D17 "Rust gateway / TS orchestration" split (referenced below) no longer holds.

---

**Status: P0–P4 complete, 2026-07-08 — the Rust gateway fully replaces the TS
one.** The box runs end to end through it (TLS termination + judge + injection
+ refresh), and `src/{gateway,judge,vault,providers,ca,schema}.ts` are retired
(only the `vault-sync` bootstrap remains in TS). `npm test` runs the Rust
tests (11 green). Next: the metering/currency/budget model with CEL on the
Rust base.

**Decision: D17 (Rust gateway) + D18 (CEL).** The trusted core becomes a Rust
binary; orchestration stays TypeScript. **Port-first**: reach parity with the
working TS gateway, then build metering/budgets on the Rust base.

## Why (recap)

The gateway now terminates TLS and parses attacker-controlled request/response
bodies, and will evaluate CEL over them on the hot path — a hostile-input
parser in the trusted core. That reverses D5's "parses no hostile input"
premise. CEL has a mature Rust implementation (`cel-interpreter`). See D17/D18.

## The split

| Rust `gateway/` (trusted core) | TypeScript `src/` (orchestration) |
|---|---|
| TLS termination, CA + leaf minting | box runner (`box.ts`), docker lockdown |
| judge + CEL, decision/call log | CLI (`cli.ts`), `vault-sync` |
| vault, OAuth refresh, injection | future supervisor, channels |
| metering, ledgers, budgets | |

Seam: the **container contract** (§7.3) and the **JSON policy/config files** +
`~/.impyard/{ca,vault}` layout — all already language-agnostic.

## What carries over unchanged

Policy/config file formats, `~/.impyard/{ca,vault}` and `runs/*.jsonl` layouts,
the box runner's behavior, every design doc/spec (box, judge, injection,
budget), and all invariants (§5). The port changes the *implementation* of one
process behind unchanged interfaces.

## Crate set (near-zero-deps spirit, but the right well-worn crates)

`tokio` (async), `rustls` + `tokio-rustls` (TLS termination with a dynamic
SNI cert resolver), `rcgen` (CA + on-the-fly leaf minting, replacing the
`openssl` shells), `hyper` + `hyper-util` + `http-body-util` (HTTP + CONNECT
proxy), `serde`/`serde_json`, `reqwest` (outbound forward + OAuth refresh),
and later `cel-interpreter`.

## Port increments (each builds + is verified before the next)

- **P0 — scaffold + CA/leaf minting (rcgen).** `gateway/` cargo binary; ensure
  the CA at `~/.impyard/ca`, mint per-host leaf certs. Unit test: a minted leaf
  carries the right SAN and verifies against the CA. *(This is where the CA's
  ownership moves: the Rust gateway generates/owns it; `box.ts` stops
  generating and just mounts the known `ca.crt` path.)*
- **P1 — MITM proxy core.** CONNECT → terminate TLS with a dynamic SNI cert →
  parse the decrypted request → forward upstream → stream the response back.
  Allow-all first; verify the box reaches `chatgpt.com` through the Rust
  gateway.
- **P2 — judge parity.** Load `policies/gateway.json` (the **structured**
  matcher — CEL comes with metering, D18), default-deny, first-match, wildcard
  host / glob tool / MCP lifting, `tunnel` escape hatch, decision log to
  `runs/decisions.jsonl`.
- **P3 — vault + injection + OAuth refresh parity.** Read `~/.impyard/vault`,
  render + inject auth headers, refresh-if-expired (single-flight, atomic
  write), `runs/credentials.jsonl`, fail-closed.
- **P4 — cut over.** Point `box.ts` / `lockdown.ts` at the Rust gateway binary
  and its `/healthz`; run the box/judge/injection acceptance suites live; then
  retire `src/{gateway,judge,vault,providers,ca}.ts`.

Then, on the Rust base: the **metering / currency / budget** model (call log →
namespaced identity → CEL currency mapping → drawdown limits) with CEL — its
own spec + increments.

## Notes

- The rcgen-generated CA replaces the current openssl one; harmless mid-dev
  (the box trusts whatever `ca.crt` is mounted). One re-`vault-sync` is not
  needed — the vault is independent of the CA.
- Keep the `--mode json` container contract identical so `box.ts` is unaffected
  except for which binary it health-checks and points the proxy at.
