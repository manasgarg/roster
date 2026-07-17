# Configuration reference

All configuration is hand-edited TOML under the config root
(`~/.config/roster`), read **live**: edits take effect on the next read,
there is no deploy step, and `roster server validate` runs the exact
loader the daemon uses and prints every error, not just the first. Broken
config fails closed — the gateway denies, dispatch pauses, `server start`
refuses to boot.

The files:

```
org.toml                    org-wide policy: grants, actions, trust, budgets,
                            memory/knowledge/context policy
workers/<name>/worker.toml  one worker: channels, heartbeat, overlays
workers/<name>/identity.md  who the worker is (prose, not config)
connections/<name>.toml     one service capability (usually wizard-written)
providers.toml              optional overlay on the built-in provider registry
```

One rule to know: `scope` is never hand-written. The loader tags everything
in `org.toml` as org-wide and everything in a worker's file as that worker's,
and rules, budgets, and trust all match subjects by ancestry.

Config that grants nothing is safe by default: no grants means no egress,
no actions means no proposals, no limits means no caps (the default-deny
judge is the security floor; budgets are an opt-in ceiling).

## org.toml

### `[engine]`

| key | default | meaning |
|---|---|---|
| `image` | `ghcr.io/manasgarg/roster-box:latest` | The box image workers run in. The server pulls it at start (re-pulled on every restart, so `:latest` stays current). Point it at a locally built tag (e.g. `roster-box`) to run your own build — a bare tag with no registry component is never pulled, only expected to exist. |
| `dir` | unset | Dev override: mount this checkout read-only over the engine baked into the box image. Unset is the normal, production posture. |

### `[[grant]]` — egress rules

Ordered; first match wins; no match denies. See [gateway.md](gateway.md)
for the full match vocabulary and semantics.

```toml
[[grant]]
name    = "model-api"
match   = { host = ["chatgpt.com", "api.anthropic.com"], port = 443 }
verdict = "allow"                      # allow | deny | tunnel
inject  = { credential = "openai-codex" }
```

| key | required | meaning |
|---|---|---|
| `name` | yes | The rule's name — budgets, trust, and audit lines bind to it |
| `match` | no (default: match everything) | Nested table: `protocol`, `host`, `port`, `method`, `pathPrefix`, `headerContains`, `maxBodySize`, `mcp = { method, tool }`; scalars or arrays |
| `verdict` | yes | `allow`, `deny`, or `tunnel` |
| `inject` | no | `{ credential = "<vault name>", provider?, headers? }` — swap the box's sentinel for the real credential in transit |

### `[[action]]` — what a worker may propose

```toml
[[action]]
name     = "email-send"
executor = "email"
trust    = "gate"            # default; "auto" skips the desk
wake_on_resolve = true       # file a continuation task when the gate resolves
```

Executors: `message-user`, `email`, `git-pr`, `identity`, `purpose`,
`discord`, `slack`, `task`, `note`. An intent with no grant is refused, not
gated. See [actions-and-trust.md](actions-and-trust.md).

### `[[trust]]` — the ladder

```toml
[[trust]]
intent = "email-send"
match  = { to = "*@ourco.com" }   # glob predicates over the payload
level  = "earned"                 # auto | gate | earned
after  = 10                       # earned: auto after N clean approvals
```

First matching rule decides. Every `match` field must hold, and a list
payload field matches only if *every* element does. `earned` promotes to
auto after `after` (default 5) executed gates with zero denials — one
denial revokes it. No rule → the action grant's own `trust` default.

### `[budget]` — currencies, meters, limits

```toml
[budget]
currencies = ["usd", "model_calls"]
vars       = { price = { model_call = 0.05 } }

[[budget.meter]]
match = 'decision.rule == "model-api"'
spend = { model_calls = "1", usd = "vars.price.model_call" }

[[budget.limit]]
currency = "usd"
window   = "day"          # minute | hour | day | month
max      = 20.0
```

Meters are CEL over `request`, `decision`, `subject`, `vars`. Limits at org
scope cap the whole fleet; per-worker limits go in the worker's file. Details and
enforcement semantics in [gateway.md](gateway.md).

### `[[expose]]` — sentinel env vars

```toml
[[expose]]
credential = "github"     # must exist in the vault (fail-closed)
env        = "GH_TOKEN"   # set in the box, to the sentinel value
```

Gives box tools an env var that *looks* authenticated; the gateway injects
the real value only where a grant's injection applies. Reserved names
(`HOME`, the proxy/CA variables, anything starting `ROSTER_`) are
rejected, as are overlapping-scope duplicates. Connections write these for
you — hand-written exposes are for credentials outside the connection
model.

### `[context]` — prompt budgets (characters)

| key | default |
|---|---|
| `max_injected_chars` | 48000 |
| `identity_max_chars` | 12000 |
| `purpose_max_chars` | 8000 |
| `briefing_max_chars` | 4000 |
| `task_max_chars` | 24000 |

Mandatory blocks fail rather than truncate; see [context.md](context.md).

### `[knowledge]`

| key | default | meaning |
|---|---|---|
| `enabled` | `true` | |
| `write_from` | `"clean-room"` | `"clean-room"`: only runs without person-data may write; `"any-run"`: scan-only legacy behavior |
| `normal_mode` | `"append"` | the only supported value |
| `max_file_chars` | 200000 | per-record cap |
| `max_repo_bytes` | 1000000000 | repo size cap |
| `checkpoint_on_clean_exit` | `true` | integrate on clean exit |
| `reorganization_requires_exclusive_lease` | `true` | must stay `true` |

### `[memory]`

| key | default | meaning |
|---|---|---|
| `enabled` | `true` | |
| `allowed_kinds` | all four | subset of `preference`, `fact`, `decision`, `interaction` |
| `max_note_chars` | 2000 | |
| `max_notes_per_scope` | 100 | |
| `recall_max_notes` | 20 | |
| `recall_char_budget` | 6000 | |
| `max_retention_days` | unset | notes expire after this many days |
| `allow_inferred_user_auto` | `false` | inferred personal facts save without review |
| `allow_worker_auto` | `false` | worker-wide notes save without review |
| `cross_channel_user_recall` | `false` | recall a user's memory outside its home channel |
| `user_memory_in_groups` | `false` | recall user memory in group contexts |

## workers/\<name\>/worker.toml

```toml
name = "yuko"                # must equal the folder name

[channels]
discord = "discord"          # vault credential for its bot (also: slack = "…")

heartbeat = "every 30m"      # the curation pulse; default 30m, "off" disables

[[budget.limit]]             # per-worker cap (limits only; currencies,
currency = "model_calls"     # vars, and meters are org-level)
window   = "hour"
max      = 60
```

A worker's file may also carry its own `[[grant]]`, `[[action]]`,
`[[trust]]`, `[[expose]]`, and `[memory]`/`[context]`/`[knowledge]`
overlays. Overlays merge over org defaults; the knowledge overlay can only
narrow (disable features, reduce limits) — it cannot re-enable, raise, or
relax what the org set. Two workers cannot bind the same channel credential.

`identity.md` sits next to it: owner-authored prose defining who the worker
is, composed into every run, editable by admins — worker-proposed edits always
gate ([actions-and-trust.md](actions-and-trust.md)).

## connections/\<name\>.toml

Usually written by `roster connection add`; hand-editable after. The file
stem is the vault credential name.

| key | required | meaning |
|---|---|---|
| `provider` | yes | registry entry (login flow + inject template), or any name if inline inject is given |
| `workers` / `scope` | one of the two | `workers = ["yuko"]` grants per-worker; `scope = "org"` grants fleet-wide |
| `hosts` | yes | allowed hostnames |
| `methods` | no (default `["*"]` — all) | allowed HTTP methods; list verbs (e.g. `["GET"]`) to narrow the grant |
| `env` | yes | the sentinel env var the box sees |
| `inject_header` / `inject_value` | together | custom injection, e.g. `"private-token"` / `"{token}"` |

A connection whose secret is missing from the vault is **disabled** — grant
and exposure omitted, loud warning in `validate` and `connection ls` — not
a fatal config error. See [connections.md](connections.md).

## providers.toml

The binary ships a provider registry: `openai-codex` and `anthropic`
(OAuth model providers), `github`, `gitlab`, `notion`, `linear` (token
capabilities), `slack` (channel *and* capability from one login), and
`discord`, `smtp` (host-side channel/email infrastructure). An entry's
supported uses come from its `use` array when present and are inferred
otherwise (a `connection` block → capability; channel auth kinds →
channel; else model). An optional `brief` string is surfaced to workers in
their run context's connections brief — the place to say how the service
is meant to be used (see [context.md](context.md)); `hidden = true` keeps an entry compiling without
showing it in the catalog. `providers.toml` overlays the registry — one
top-level table per provider, each entry **replacing** that provider's
default wholesale (`roster connection add --declare` writes these for
you):

```toml
[acme]
auth = "api_key"
inject = [{ header = "authorization", value = "Bearer {key}" }]
connection = { hosts = ["api.acme.com"], env = "ACME_TOKEN" }
```

## Environment variables

| variable | meaning |
|---|---|
| `ROSTER_ROOT` | self-contained mode: everything under `$ROSTER_ROOT/{config,data,state}` — tests, scratch deployments, side-by-side instances |
| `XDG_CONFIG_HOME` / `XDG_DATA_HOME` / `XDG_STATE_HOME` | standard XDG overrides for the three roots |
| `ROSTER_VAULT_DIR`, `ROSTER_CA_DIR` | relocate the vault or CA individually |
| `ROSTER_EMAIL_SINK` | testing: write outbound email to `state/outbox/` instead of SMTP |
