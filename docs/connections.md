# Service connections

A **connection** is one intent — "this worker may act on that service" — as
one first-class object. Behind it sit four moving parts (a provider's login
flow, a secret in the vault, an egress grant with credential injection, and
the env var the box sees), and the connection keeps them coherent so you
never assemble them by hand:

```toml
# ~/.config/roster/connections/github.toml
provider = "github"          # registry entry: login flow + inject template
workers     = ["yuko"]          # or: scope = "org" (the explicit escalation)
hosts    = ["api.github.com"]
methods  = ["GET"]           # writes are a deliberate manual edit
env      = "GH_TOKEN"        # what the box sees (a sentinel, never the secret)
```

The loader compiles each connection, live, into an egress grant with
injection plus an env exposure. The file name is the vault credential name.

Two structural guarantees:

- **No ordering footgun.** Connection grants are spliced before all
  hand-written grants (first match wins), so a broad rule like `web-fetch`
  (GET on `*`) can never shadow a connection's injection.
- **No sequencing trap.** A connection whose secret is missing from the
  vault is *disabled* — grant and exposure omitted, loud warning in
  `validate`, `server start`, and `connection ls` — instead of failing the
  whole config closed. (Hand-written `[[expose]]` keeps strict fail-closed
  semantics.)

## One command

```bash
roster connection catalog
roster connection add                        # bare: shows the catalog
roster connection add github --worker yuko      # login → vault → scaffold → validate
roster connection add github --org           # org-wide, spelled out
roster connection add github --name github-kdemo --worker kdemo
```

The wizard runs the provider's login flow, stores the secret, scaffolds the
connection file (once — re-running only **rotates the secret**, never
touches your edits), and prints the compiled result. Per-worker is the default
posture: a connection is a capability granted to an identity, not to the
fleet. `--name` gives the connection its own name — the idiom for per-worker
service identities (separate PATs mean the service's own audit log
distinguishes your workers too).

Inventory: `roster connection ls [--json]` — provider, scope, hosts, env,
active/DISABLED.

## Scope rules

- **Services are box-consumed capabilities** → per-worker by default.
- **Channels (discord, slack, smtp) are host-consumed infrastructure.**
  `roster credential add discord` stores the credential; bind it in the
  worker's `[channels]` table ([channels.md](channels.md)). The credential
  never enters a box, so it is not a connection.
- **Model providers** (anthropic, openai-codex) are wired via grants with
  `inject` — see [gateway.md](gateway.md).

## The catalog

Presets ship in the binary's provider registry: **github, gitlab,
slack-api, notion, linear** — each with its auth kind, inject template,
canonical hosts, and conventional env var. (`slack` is the *channel*
provider; `slack-api` is the service. Talking in Slack and calling Slack
are different intents and get different names.)

Presets, not a restriction — connect any token-authenticated API by naming
its host. Roster prompts for the token without echoing it and defaults to
`Authorization: Bearer {token}`, GET-only, and an env var derived from the
name:

```bash
roster connection add acme --host api.acme.com --worker yuko
```

Override the defaults for APIs with different conventions:

```bash
roster connection add gitlab-internal \
  --host gitlab.example.com \
  --header 'Private-Token: {token}' \
  --env GITLAB_TOKEN \
  --method GET --method POST \
  --worker yuko
```

For a reusable preset with a custom login flow, declare it in
`providers.toml` ([configuration.md](configuration.md)):

```toml
[acme]
auth = "api_key"
inject = [{ header = "authorization", value = "Bearer {key}" }]
connection = { hosts = ["api.acme.com"], env = "ACME_TOKEN" }
```
