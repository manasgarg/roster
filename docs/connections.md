# Connections

A **connection** is roster's relationship with an external service: an
identity (a secret in the vault) plus one or more **uses**. You perform one
mental act — "hook roster up to service X" — and roster routes the result
to whoever consumes it. There is no separate "credential" vocabulary; the
vault is an implementation detail.

Three uses exist. Each has a home, and `connection ls` derives a
connection's uses from what actually references its secret — nothing is
stored that could drift:

| Use | What connecting gives you | Where the binding lives |
|---|---|---|
| **capability** | the worker's box may act on the service: egress grant + credential injection + env sentinel | `connections/<name>.toml` |
| **channel** | the worker speaks through the service (Discord/Slack listeners, SMTP send) | `[channels]` in worker.toml |
| **model** | the gateway injects it into model-API calls | `inject` on grants |

Whatever the use, the secret never enters a box: capabilities get
sentinels with injection in transit ([gateway.md](gateway.md)); channel
and model secrets are consumed host-side only.

## One command

```bash
roster connection catalog
roster connection add                       # bare: the guided session
roster connection add github --worker yuko  # login → vault → scaffold → validate
roster connection add discord --worker yuko # login → vault → [channels] binding
roster connection add anthropic             # login → vault → grant report
roster connection add slack --worker yuko   # one login, channel AND capability
roster connection add github --org          # org-wide, spelled out
roster connection add github --name github-kdemo --worker kdemo
```

`add` runs the provider's login flow (paste details, or an OAuth dance
roster triggers), stores the secret, and follows through per use:

- **capability** — scaffolds the connection file (once — re-running only
  **rotates the secret**, never touches your edits) and prints the
  compiled result. Per-worker is the default posture: a connection is a
  capability granted to an identity, not to the fleet. `--name` gives the
  connection its own name — the idiom for per-worker service identities.
  The worker is told: every run's system context carries a compiled
  connections brief — each applicable connection's hosts, methods, and env
  stand-in, plus the provider's `brief` usage line from the registry (e.g.
  github's says to work through the API; plain `git` isn't authenticated).
  Override or add `brief` per provider in providers.toml.
- **channel** — offers the `[channels]` binding and writes it into the
  chosen worker's spec (`--worker` answers non-interactively; declining
  prints the snippet). One credential serves one worker's listener —
  use `--name` for a second bot.
- **model** — a grant by default: scaffolds a connection file whose hosts
  derive from the provider registry, compiling into an
  allow-and-inject rule for the model API (org-wide unless `--worker`
  narrows it, no env exposure). The file is admin-owned after creation —
  edit or delete it to change access; a hand-written `[[grant]]` injecting
  the same credential is respected and nothing is scaffolded over it.

A provider supporting several uses (slack) asks which to set up —
`--use channel --use capability` for scripts — and collects only the
fields those uses need. Talking in Slack and calling the Slack API are one
connection now: the listener consumes the bot + app tokens, the capability
injects `Bearer {bot_token}` for `slack.com`.

Inventory and removal:

```bash
roster connection ls [--json]   # every connection, use(s), state
roster connection rm <name>     # delete the secret; reports what still references it
```

`ls` states: **active** (secret present, use bound), **DISABLED (no
secret)** (a reference exists but the vault has nothing — grant and
exposure omitted, warned loudly, never fail-open), **unbound** (a secret
nothing references; the natural state mid-setup, named so orphans are
visible). `rm` never edits config — it deletes the secret and tells you
exactly which files still reference it.

## Two structural guarantees

- **No ordering footgun.** Connection grants are spliced before all
  hand-written grants (first match wins), so a broad rule like `web-fetch`
  (GET on `*`) can never shadow a connection's injection.
- **No sequencing trap.** A connection whose secret is missing from the
  vault is *disabled* — grant and exposure omitted, loud warning in
  `validate`, `server start`, and `connection ls` — instead of failing the
  whole config closed. (Hand-written `[[expose]]` keeps strict fail-closed
  semantics.)

## The catalog

Presets ship in the binary's provider registry, grouped by what connecting
gives you: capabilities (**github, gitlab, notion, linear, slack**),
channels (**discord, slack, smtp**), models (**anthropic, openai-codex**).
Each entry carries its auth kind, inject template, canonical hosts, and
conventional env var.

Presets, not a restriction — connect any token-authenticated API by naming
its host. Roster prompts for the token without echoing it and defaults to
`Authorization: Bearer {token}`, all methods (`methods = ["*"]` — connecting
a service grants the service, not a verb subset), and an env var derived
from the name:

```bash
roster connection add acme --host api.acme.com --worker yuko
```

Override the defaults for APIs with different conventions — `--method`
narrows the grant (e.g. read-only):

```bash
roster connection add gitlab-internal \
  --host gitlab.example.com \
  --header 'Private-Token: {token}' \
  --env GITLAB_TOKEN \
  --method GET \
  --worker yuko
```

## Unknown services: the interview

Bare `roster connection add` opens on the catalog; name something it
doesn't know and roster interviews you for what a registry entry would
have said. Key-shaped services stay a one-off — the collected hosts,
header template, and env var live in the connection file itself. OAuth is
**kind-knowledge** (login endpoints, refresh, client id) and lands as a
`providers.toml` entry, shared by every connection to that service and
read by the gateway's token refresh:

```bash
roster connection add acme --declare    # the interview, name fixed
```

OAuth against an arbitrary host is impossible without an app registration
(client id, endpoints), so the interview asks for yours — roster ships no
hosted redirect and no client registrations of its own. A declared entry
is commented and human-owned after; the file remains the authoring surface
for anything the interview didn't ask
([configuration.md](configuration.md) documents the format):

```toml
[acme]
auth = "oauth"
client_id = "…"
token_url = "https://auth.acme.com/token"
token_encoding = "json"
inject = [{ header = "authorization", value = "Bearer {access}" }]
connection = { hosts = ["api.acme.com"], env = "ACME_TOKEN" }

[acme.login]
flow = "pkce"
authorize_url = "https://auth.acme.com/authorize"
redirect_uri = "http://localhost:1455/callback"
scope = "read write"
```
