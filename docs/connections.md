# Connections

A **connection** is roster's relationship with an external service: an
identity (a secret in the vault) plus one or more **uses**. You perform one
mental act — "hook roster up to service X" — and roster routes the result
to whoever consumes it. There is no separate "credential" vocabulary; the
vault is an implementation detail.

Connections also cover **host resources**: a directory or a git
repository on the host is added the same way (`kind = "host-dir"` /
`"host-repo"` in the connection file) and granting one materializes it in
the worker's filesystem under `$HOME/mnt/<name>`. Everything a box can
touch arrives as a connection grant; nothing is ambient.

Four uses exist. Each has a home, and `connection ls` derives a
connection's uses from what actually references it — nothing is stored
that could drift:

| Use | What connecting gives you | Where the binding lives |
|---|---|---|
| **capability** | the worker's box may act on the service: egress grant + credential injection + env sentinel | `connections/<name>.toml` |
| **channel** | the worker speaks through the service (Discord/Slack listeners, SMTP send) | `[channels]` in worker.toml |
| **model** | the gateway injects it into model-API calls | `inject` on grants |
| **mount** | the resource appears in the worker's filesystem at `$HOME/mnt/<name>` | `kind = "host-dir"` / `"host-repo"` in the connection file |

Whatever the use, the secret never enters a box: capabilities get
sentinels with injection in transit ([gateway.md](gateway.md)); channel
and model secrets are consumed host-side only.

## One command

```bash
roster connection catalog
roster connection add                       # bare: the guided session
roster connection add github                # login → vault → scaffold (granted to no one yet)
roster connection add github --worker dobby  # add + grant in one breath
roster connection add anthropic             # login → vault → grant report
roster connection add slack --worker dobby   # one login, channel AND capability
roster connection add github --org          # org-wide, spelled out
roster connection add github --name github-kdemo --worker kdemo
```

Two acts, two verbs. `add` is the roster-level act: it connects the org
to the service (login, vault, connection file). `grant` is the
availability act: it makes the connection usable by a worker, and the
restriction rides on the edge:

```bash
roster connection grant github dobby                          # unrestricted edge
roster connection grant discord dobby --restrict servers=999  # scoped edge
roster connection grant github --org                         # fleet-wide edge
roster connection revoke discord dobby                        # withdraw the edge
```

`add --worker` / `add --org` are sugar for add-then-grant; bare `add`
leaves the connection granted to no one — a legal resting state, shown as
**ungranted** in `ls`.

`add` runs the provider's login flow (paste details, or an OAuth dance
roster triggers), stores the secret, and follows through per use:

- **capability** — scaffolds the connection file (once — re-running only
  **rotates the secret**, never touches your edits) and prints the
  compiled result. `--name` gives the connection its own name — the idiom
  for per-worker service identities.
  The worker is told: every run's system context carries a compiled
  connections brief — each applicable connection's hosts, methods, and env
  stand-in, plus the provider's `brief` usage line from the registry (e.g.
  github's says to work through the API; plain `git` isn't authenticated).
  Override or add `brief` per provider in providers.toml.
- **channel** — offers the `[channels]` binding and writes it into the
  chosen worker's spec (`--worker` answers non-interactively; declining
  prints the snippet). The connection is named after the bot the token
  authenticates — discord's `users/@me`, slack's `auth.test` — so a bot
  named "looper" lands as `discord-looper` and a second bot never
  collides with the first (`--name` overrides; if the identity can't be
  probed, the wizard falls back to the service name and asks before
  rotating a credential a listener consumes). One credential serves one
  worker's listener.
- **model** — a grant by default: scaffolds a connection file whose hosts
  derive from the provider registry, compiling into an
  allow-and-inject rule for the model API (org-wide unless `--worker`
  narrows it, no env exposure). The file is admin-owned after creation —
  edit it, use grant/revoke, or delete it to change access; a hand-written
  `[[grant]]` injecting the same credential is respected and nothing is
  scaffolded over it.

A provider supporting several uses (slack) asks which to set up —
`--use channel --use capability` for scripts — and collects only the
fields those uses need. Talking in Slack and calling the Slack API are one
connection now: the listener consumes the bot + app tokens, the capability
injects `Bearer {bot_token}` for `slack.com`.

Inventory and removal:

```bash
roster connection ls [--json]   # every connection, use(s), state
roster connection rm <name>     # delete the secret and (on confirm) the connection file
```

`ls` states: **active** (secret present, edge granted), **ungranted**
(connected, granted to no one — the resting state between `add` and
`grant`), **DISABLED (no secret)** (a reference exists but the vault has
nothing — grant and exposure omitted, warned loudly, never fail-open),
**unbound** (a secret nothing references; the natural state mid-setup,
named so orphans are visible). `rm` deletes the secret, then offers to
delete the connection file — edges die with it (each step behind its own
y/N). It never edits org.toml or worker specs — references there are
reported for you to remove yourself, and `revoke` owns a worker's
`[channels]` binding.

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
roster connection add acme --host api.acme.com --worker dobby
```

Override the defaults for APIs with different conventions — `--method`
narrows the grant (e.g. read-only):

```bash
roster connection add gitlab-internal \
  --host gitlab.example.com \
  --header 'Private-Token: {token}' \
  --env GITLAB_TOKEN \
  --method GET \
  --worker dobby
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

## Plain git to github.com

The `github` preset covers both `api.github.com` — the gh CLI, REST,
GraphQL — and `github.com`, git's own endpoints: `git clone`, `fetch`, and
`push` work from a box by default. The registry injects per host from the
same stored token — `token {key}` on the API, Basic (git's smart-HTTP
scheme) on `github.com` — so the box still never sees the credential, and
every request is judged and audited like any other. Narrowing is one line
in the connection file — drop `github.com` from `hosts` and git is refused
while the API keeps working:

```toml
# connections/github.toml
hosts = ["api.github.com"]   # API only; plain git refused
```

Weigh what you are choosing. Direct git bypasses the gated-repo lane
entirely ([repos.md](repos.md)): no validated push, no participant scan,
no fast-forward-only `main`, no quarantine backstop — and a grant does not
distinguish run kinds, so a channel session can push exactly as a task run
can. The token's scope is the blast radius: whatever it can reach, any
granted worker can now write. A fine-grained PAT scoped to the
repositories under development, plus branch protection so `main` moves
only by pull request (workers push branches and open PRs through the API
they already have), keeps the capability while narrowing what one bad
push can do. Both lanes coexist: gated host-repos where governance earns
its keep, direct git where development speed wins.

## Host connections: directories and repositories

A host path becomes a connection the same way a service does — a file in
`connections/`, granted per worker or org-wide:

```toml
# connections/notes.toml — a directory
kind = "host-dir"
path = "/home/you/shared-notes"
mode = "ro"                   # or "rw"

[grant.dobby]                  # membership only — mounts have no dimensions
```

```toml
# connections/research-kb.toml — a git repository
kind  = "host-repo"
path  = "/home/you/research-kb.git"
write = "gated"               # or "ro"; gated needs a bare repo
# write_from = "any-run"      # this repo's own write contract; default:
                              # the org [knowledge] write_from (repos.md)

[grant.org]                   # the fleet-wide edge
```

Granting one mounts it at `$HOME/mnt/<name>` in every run: `host-dir` as
a plain bind (ro or rw), `host-repo` either read-only or — gated — as a
per-run clone whose branch lands through the validated `repo_push`
action ([repos.md](repos.md)). The connection name is the mount
directory, so names are lowercase path-safe words.

Two cautions the loader enforces or voices: a missing path fails config
closed (a mount the worker was promised must exist), and an `rw` dir
grant warns that roster does not back it up — unlike the worker's own
[store](store.md), which gets snapshots and restore, a bad run's writes
to a granted dir are unrecoverable by roster.

## Scoping a grant

Each `[grant.<worker>]` edge carries its own scope in provider-declared
dimensions — two workers on one connection can see different slices of
the service. One scope governs every use the edge has: the listener
refuses to attach outside it AND the gateway compiles it into request
predicates, so "can speak there" and "can act there" never drift apart:

```toml
# connections/discord.toml
provider = "discord"
env      = "DISCORD_TOKEN"
hosts    = ["discord.com"]

[grant.dobby]
servers  = ["1015381923845"]      # a guild id
surfaces = ["1451951375079"]      # and/or specific channel ids

[grant.kdemo]
surfaces = ["public", "dm"]       # classes: where kdemo exists, by shape
```

`[grant.org]` is the fleet-wide edge; a worker's own edge wins over it.
The CLI writes these for you (`roster connection grant discord dobby
--restrict servers=…`), and the edit is live either way. A worker with no
edge gets nothing: the listener drops its guild traffic and no rules
compile. (The pre-edge form — `workers = [..]`/`scope = "org"` with one
shared `[restrict]` — still parses as identical edges, and `grant`
migrates such a file the first time it touches one. `channels =` is the
pre-rename spelling of `surfaces =` and still parses.)

Discord declares `servers` and `surfaces` (the provider registry's
`scope_dims`). The surfaces list mixes **ids** and **classes** —
`public`, `private`, `dm`, validated against the registry's
`surface_classes` — and every entry admits: a listed channel works even
when its server isn't listed, a listed server admits all its channels, a
listed class admits every surface the listener classifies as that class
(Discord: a guild channel whose permission overwrites hide it from
@everyone is `private`, the rest are `public`).

**DMs are admitted by default** (1:1, sought-out, dynamically created ids
that could never be pre-listed). A scope that names classes is
exhaustive: `surfaces = ["public"]` means no DMs — the first way to say
"this worker does not do DMs" — while an id-only or servers-only scope
keeps the default.

On the gateway side, a surface-id restriction compiles to path predicates
on the Discord API (allow the scoped channels, deny the rest). Classes
can't compile statically — a URL doesn't reveal a channel's class — so
they compile to a dynamic predicate: the judge reads the listener's
recorded classification of the channel named in the path, and a channel
the listener never classified matches nothing (fail closed — the deny
behind it wins). Both enforcement points read one classification, so
"can speak there" and "can act there" stay aligned; the same rule keeps
DM sends working under an id-only scope (DMs admitted by default). The
one remaining broad spot is a servers-scoped grant: Discord channel
endpoints don't carry the guild id, so there the listener's attachment
rule is the enforcement and the gateway stays wide on `/channels`.

There is no universal scope language: a provider declares its dimensions
in the registry, and they compile down to the two enforcement points that
already exist. For a generic HTTP capability the scope *is* the grant
vocabulary — hosts, methods, paths.
