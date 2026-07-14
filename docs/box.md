# The box

Workers run in the **box**: a Docker container that is deliberately capable
inside and powerless at the edge. Inside, pi has a real toolchain — git,
`gh`, Python, Node, build tools. At the edge, there is no internet, no DNS,
no secrets, and no writable code. Capability and authority live in
different layers: being useful is not the same as being trusted.

A "no" is always legible — a 403 naming the rule, a 402 naming the budget —
never a missing package, an untrusted certificate, or a hung socket.

## The lockdown

**No route.** The box joins `roster-locked`, a Docker bridge network with
IP masquerade disabled: packets to the internet leave with an address
replies can't route back to, so outbound connections simply never complete.
The host stays reachable — that's where the gateway is.

**No DNS.** With an HTTP proxy, the box needs no resolver at all — clients
hand the hostname to CONNECT and the gateway resolves it. The box runs with
`--dns 127.0.0.1`, a blackhole, so DNS can't be used as an exfiltration
side channel. Tools that insist on resolving before proxying fail fast
instead of hanging.

**One door.** `HTTP_PROXY`/`HTTPS_PROXY` (and friends) point at the gateway,
carrying the run's single-use identity token as the proxy credential — how
every request gets attributed, un-spoofably, to this worker and run
(see [gateway.md](gateway.md)).

**Fail closed.** If the locked network can't be ensured, the gateway isn't
answering, the CA is missing, or there are no model credentials to
sentinel, the runner refuses to start the box. It never falls back to open
egress.

**TLS that just works.** The box trusts the gateway's CA everywhere:
`NODE_EXTRA_CA_CERTS` gets the CA certificate, and everything that replaces
its root store (`SSL_CERT_FILE`, `CURL_CA_BUNDLE`, `REQUESTS_CA_BUNDLE`,
`GIT_SSL_CAINFO`, `PIP_CERT`) gets a combined bundle — system roots plus the
roster CA — so Go, Rust, git, curl, and pip all work through interception,
and tunneled hosts presenting real certificates still verify.

## Nothing worth stealing

No real credential enters the box, ever:

- The model auth in the box's home is a **sentinel** — same shape as the
  real thing (a well-formed fake JWT where a JWT is expected), useless
  everywhere. The gateway swaps in the real token in transit.
- Service tokens exposed via connections or `[[expose]]` appear as env vars
  set to the sentinel string. `GH_TOKEN` works exactly where policy says
  GitHub calls may go, and nowhere else — leaking the entire box
  environment leaks placeholders.
- Bot tokens, SMTP credentials, and the CA private key are host-side only.

And nothing to tamper with: the box mounts none of the deployment — not
config, not data, not state. It sees only its own run directories
(workspace, session, home — read-write), its knowledge checkout (read-write
or read-only by provenance, see [knowledge.md](knowledge.md)), its
channel's history when it has one (read-only), a code worktree for code
tasks, and the CA certificates (read-only).

`/tmp` is a private 2 GiB tmpfs that vanishes with the container —
downloads and scratch work go there and are never mistaken for durable
storage.

Runs end at a hard wall-clock ceiling (`docker kill` — default 30 minutes,
per-run `--ceiling`); warm chat sessions are bounded by their idle window
instead (see [channels.md](channels.md)).

## The toolbelt

The image (`box/Dockerfile`, tag `roster-box`) is `node:24-bookworm-slim`
plus the tools a worker actually reaches for. On a dead network, more
binaries add no egress power — the cost is only image size:

- **VCS / GitHub**: git, `gh`, git-lfs
- **Transfer & archives**: curl, wget, rsync, unzip, zip, xz, zstd
- **Data**: sqlite3, jq, `yq`, poppler-utils (pdftotext)
- **Search & files**: ripgrep, fd, tree, file, less
- **Python**: python3, pip, venv, `uv`
- **Node**: node 24, npm, pnpm/yarn via corepack
- **Build**: build-essential, pkg-config (native pip/npm extensions)
- **Misc**: openssl, procps, moreutils, tzdata

`--build-arg TIER2=1` adds pandoc, imagemagick, and ffmpeg for
document/media work. Go and Rust toolchains and a headless browser are
deliberately not defaults — add them per deployment if a worker's job needs
them.

## The engine and its tools

pi and the box extensions are **baked into the image** at
`/opt/roster/engine` — the box does not depend on your checkout. (For
development, `[engine] dir` in `org.toml` mounts a checkout read-only over
the baked engine.)

The extensions are the worker's hands, in two files under `box/extensions/`:

- **`web.ts`** — governed retrieval: `web_search` (keyless DuckDuckGo
  search) and `fetch_pages` (fetch and extract readable markdown). Plain
  HTTP through the proxy; every request judged and logged.
- **`actions.ts`** — proposals, not powers: `message_user`, `discord_send`,
  `slack_send`, `send_email`, `propose_changes`, `propose_purpose_edit`,
  `file_task`, the memory tools (`remember`, `forget_memory`,
  `correct_memory`, pin/disable variants, `read_memory`,
  `set_memory_preferences`), and `check_gates`. Each submits a typed
  envelope to the gateway's action host and obeys the verdict that comes
  back — the tools shape behavior; they never enforce safety
  (see [actions-and-trust.md](actions-and-trust.md)).

Dropping a new `.ts` file into `box/extensions/` ships a new capability;
governance for whatever it does comes free, because the gateway is still
the only exit.
