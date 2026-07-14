# The capable box (spec, 2026-07-13)

**Status: fully implemented and conformance-verified 2026-07-13**
(e52da21 bundle+DNS, 4cc86e7 verdicts, 6a3ea9d toolbelt, 2df6631 expose,
plus 2b: pi baked into the image; `[engine] dir` is now a dev override and
the deployment runs without it — the box no longer mounts the checkout). Conformance:
curl/python/node/gh all reached an allowed host through the gateway (gh got a
real GitHub 401 for a dummy token — proxy, bundle TLS, and policy all held);
an ungranted POST returned 403 with `x-impyard-verdict: deny`; a raw socket to
1.1.1.1:443 timed out; in-box DNS fails fast; the env held zero
secret-looking vars; every probe appears attributed in decisions.jsonl.

**Principle.** Capability and authority live in different layers. Inside the
box, pi should have every tool a competent imp needs, and every byte it
sends should reach the gateway *by convention* — inherited env vars, one trust
bundle, protocols the proxy speaks. At the gateway, org.toml is the entire
description of what an imp may do. A "no" is always a legible verdict, never
a missing package, an untrusted CA, or a hung socket. Enforcement continues to
come from topology (the NAT-less network), not from starving the box.

**Invariants (unchanged, verified after each increment):**

1. The box never holds a real credential — sentinels only, injection in transit.
2. The only network exit is the gateway; direct egress hangs.
3. Attribution is the per-run identity token, un-spoofable by content.
4. Every governed call is judged, metered, and logged.
5. The trust boundary is the process/language boundary (D20): TS in the box,
   decisions in Rust on the host.

---

## 1. One trust bundle for every ecosystem  (S)

Today only Node, curl, and Python requests trust the impyard CA (three env
vars). Go, Rust, git, and anything reading the system store fail TLS against
terminated connections — an illegible "no".

- At CA mint time (`Ca::ensure`), also write `ca/bundle.crt` = the host's
  system roots (`/etc/ssl/certs/ca-certificates.crt`) + `ca.crt` appended.
  Regenerate whenever either input is newer.
- Mount it at `/opt/impyard/ca-bundle.crt` (keep the bare `ca.crt` mount).
- Point everything at the bundle: `SSL_CERT_FILE` (Go, OpenSSL, rustls),
  `CURL_CA_BUNDLE`, `REQUESTS_CA_BUNDLE`, `GIT_SSL_CAINFO`, `PIP_CERT`;
  `NODE_EXTRA_CA_CERTS` stays on `ca.crt` (it is additive).
- Why a bundle and not just the impyard CA: `SSL_CERT_FILE` *replaces* default
  roots, and tunnel-verdict hosts (cert-pinning clients) present real
  certificates that still need real roots.

### 1b. Close the DNS side door

With an HTTP proxy, the box needs no DNS at all — clients hand the hostname to
CONNECT and the gateway resolves it. Docker's embedded resolver currently
forwards external lookups via the host daemon: a functioning low-bandwidth
exfiltration channel. Run boxes with `--dns=127.0.0.1` (a blackhole; nothing
listens there). `host.docker.internal` is unaffected — it's an `/etc/hosts`
entry, not DNS. Risk: a tool that resolves before proxying breaks; that tool
was already broken (resolution succeeded, connect hung) — now it fails fast.
Rollback is removing one flag.

## 2. The toolbelt  (M)

Bake the tools an imp actually reaches for into `box/Dockerfile`, chosen and
patched deliberately instead of fetched ad hoc into /tmp. On the dead network,
more binaries add no egress power; the costs are image size and CVE cadence.

**Tier 1 — always installed** (apt unless noted; base is node:24-bookworm-slim,
which brings node/npm/npx; bash, git, curl, ripgrep, python3, jq,
ca-certificates already present):

| Purpose | Packages |
|---|---|
| GitHub / VCS | `gh` (official apt repo), `git-lfs` |
| Transfer | `wget`, `rsync` |
| Archives | `unzip`, `zip`, `xz-utils`, `zstd` |
| Data | `sqlite3`, `yq` (mikefarah static binary), `poppler-utils` (pdftotext/pdfinfo) |
| Files/search | `fd-find`, `tree`, `file`, `less` |
| Python | `python3-pip`, `python3-venv`, `uv` (COPY --from astral-sh/uv) |
| Build | `build-essential`, `pkg-config` (native pip/npm extensions; ~250 MB — the single biggest line, and the most common illegible failure when absent) |
| Debug/misc | `openssl`, `procps`, `moreutils`, `tzdata`; `corepack enable` (pnpm/yarn) |

**Tier 2 — org choice, behind a build arg** (`--build-arg TIER2=1`):
`pandoc`, `imagemagick`, `ffmpeg`. Each is large; an admin who runs document/
media imps turns them on.

**Explicit non-defaults:** Go/Rust toolchains (heavy; add per-deployment if a
imp's job is Go/Rust), headless browser/playwright (~400 MB and its own
proxy/CA story — future increment if research imps need real browsing).

Estimated image: ~220 MB (base) → ~650–700 MB tier 1.

### 2b. Bake pi + extensions into the image

pi is the most important utility of all and is still bind-mounted from the
checkout (`[engine] dir`). Move it into the image: build arg `PI_VERSION`,
`npm ci` into `/opt/impyard/engine` at build, extensions copied alongside.
`[engine] dir` becomes an optional dev override (mount wins when set);
`engine_fingerprint` reads the image label instead of hashing the checkout.
`server validate`'s "engine dir unset" note inverts into the happy path.

## 3. Generalized sentinel auth  (M)

The pattern that lets pi call models — sentinel in the box, real key injected
in transit — is bespoke today. Generalize it so any CLI can be *authenticated
under policy*:

- Config: a credential exposure list, org- or imp-scoped:

  ```toml
  [[expose]]
  credential = "github"      # vault name
  env = "GH_TOKEN"           # set in the box to the sentinel value
  ```

- Provisioning sets each exposed env to the shared sentinel.
- The gateway already injects per grant (`inject = { credential = … }`); the
  sentinel in an outbound header is a placeholder the substitution rewrites.
  The grant's host/method scope decides where the real value appears — an
  exposed `GH_TOKEN` works exactly where policy says GitHub calls may go,
  and nowhere else. Leaking the box env leaks nothing.
- `server validate` lists exposures; unknown credential names are config
  errors (fail closed, like listener credentials).

## 4. Legible verdicts  (S)

The proxy already returns `403 {"error":"denied by gateway (deny)","rule":…}`
and `402 {"error":"budget exceeded","detail":…}`. Finish the job:

- Add `X-Impyard-Verdict: deny|budget` and `X-Impyard-Rule: <name>` headers so
  CLIs that print only status lines still surface the reason.
- Add a `hint` field: deny → "this is policy, not an outage — propose an
  action or ask your lead"; budget → include the window and `Retry-After`.
- Actions (`actions.impyard.internal`) already speak in sentences; no change.
- One line in the runtime policy's gateway paragraph: the response itself
  names the rule.

## Sequencing

1. Trust bundle + DNS blackhole (1, 1b) — small, unlocks every Go/Rust tool.
2. Verdict headers/hints (4) — small, pairs with the runtime-policy text.
3. Toolbelt tiers (2) — one Dockerfile pass + rebuild.
4. Sentinel exposure (3) — config schema + provisioning + validate.
5. Bake pi (2b) — separate increment; retires `[engine] dir`.

## Verification

A conformance task run inside a real box (`imp run`), asserting each layer:

- `curl`, `python3 -c` (urllib), `node -e` (fetch), and `gh api` GET an
  allowed host → 200 through the gateway.
- The same clients POST to an ungranted host → 403 whose body names the rule.
- Over-budget call → 402 with `Retry-After`.
- Raw `python3` socket to 1.1.1.1:443 → fails fast (no route, no DNS).
- `env | grep -c TOKEN` shows sentinels only; decisions.jsonl shows every call
  above, attributed to the run.

Plus `cargo test` for config parsing (expose list, validation errors) and the
bundle-generation logic.
